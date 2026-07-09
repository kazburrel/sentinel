//! Minimal-but-safe HTTP receiver for the ESP32 -> WiFi -> Mac upload path.
//!
//! Milestone 12 proved the pipe works at all; Milestone 13 hardened it per
//! the project's security review before any real camera footage got wired
//! in: auth token, bounded header/body sizes, a read timeout, exact-body
//! validation (no 200 on a truncated upload), and safe server-chosen
//! filenames under one dedicated storage directory -- never a
//! client-supplied path. Still deliberately not a real router/framework;
//! just `POST /upload` with a token, nothing else.
//!
//! Milestone 14 replaced the raw-bytes-to-one-file storage with the
//! `shared` crate's event envelope: the body is now a small header
//! followed by one or more named, typed parts (thumbnail JPEG now,
//! recorded video later, audio optional after that), each written to its
//! own file under `UPLOADS_DIR`.
//!
//! Milestone 19 added AI thumbnail analysis (`ai` module): after an event
//! is stored *and* the firmware upload has already been answered, the
//! thumbnail is sent to a local Ollama vision model and the result saved
//! as `analysis.json` beside the event's files. This is deliberately
//! decoupled from the upload response -- Ollama being offline, slow, or
//! wrong can never affect whether an event upload succeeds.
//!
//! Only acceptable on a trusted LAN during development -- see
//! `PROJECT_STATUS.md`'s security threat model for what's still needed
//! before this is exposed beyond that (TLS, network segmentation).

mod ai;
mod config;
mod retention;

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shared::{decode_envelope_header, EnvelopeError, PartHeader, PartKind, PartsIter};

const PORT: u16 = 8080;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Comfortably above the largest clip Milestone 10/11 measured (~8MB PSRAM,
/// ~35-50s of footage) without being unbounded.
const MAX_BODY_BYTES: usize = 12 * 1024 * 1024;
// An absolute path baked in at compile time, anchored to this crate's own
// directory -- not a path relative to the process's current working
// directory, which depends on how the binary happens to be launched (e.g.
// `cargo run -p server` from the workspace root vs. `cargo run` from inside
// `server/` land uploads in different places otherwise).
const UPLOADS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/uploads");
/// Bounds one Ollama analysis call -- a slow/hung local model must not be
/// able to block the analysis pass forever (it already can't block the
/// firmware upload response at all; see the `ai` module doc).
const AI_ANALYSIS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
enum RequestError {
    /// Field is only ever read via this enum's own `Debug` impl (for the
    /// `println!("request failed: {e:?}")` log line), which the dead-code
    /// lint doesn't credit as a use.
    #[allow(dead_code)]
    Io(std::io::Error),
    HeadersTooLarge,
    ConnectionClosedEarly,
    BadRequestLine,
    MissingContentLength,
    BodyTooLarge,
    Unauthorized,
    BodyTruncated,
    /// Same dead-code-lint caveat as `Io` above: read via `Debug` in the
    /// `println!` log line and in test assertions, neither of which the
    /// lint credits as a use.
    #[allow(dead_code)]
    InvalidEnvelope(EnvelopeError),
    /// A storage-layer I/O failure (disk full, permissions, etc.) --
    /// deliberately distinct from `Io` (TCP/connection failures) so it maps
    /// to `500`, not `400`: the request itself was a perfectly valid event,
    /// the server just failed to persist it, so firmware should treat this
    /// as retryable rather than as its own fault.
    #[allow(dead_code)]
    StorageFailure(std::io::Error),
}

impl From<std::io::Error> for RequestError {
    fn from(e: std::io::Error) -> Self {
        RequestError::Io(e)
    }
}

impl RequestError {
    fn status_line(&self) -> &'static str {
        match self {
            // Unknown route and failed auth (missing or wrong token) return
            // the exact same generic 404 -- deliberately indistinguishable,
            // so a client probing for valid paths/tokens can't tell "wrong
            // path" from "wrong token" and confirm /upload is a real
            // authenticated endpoint. This is a concealment layer on top of
            // the actual auth check, not a substitute for it.
            RequestError::BadRequestLine | RequestError::Unauthorized => "HTTP/1.1 404 Not Found",
            RequestError::Io(_) | RequestError::ConnectionClosedEarly => "HTTP/1.1 400 Bad Request",
            RequestError::HeadersTooLarge => "HTTP/1.1 431 Request Header Fields Too Large",
            RequestError::MissingContentLength => "HTTP/1.1 411 Length Required",
            RequestError::BodyTooLarge => "HTTP/1.1 413 Payload Too Large",
            RequestError::BodyTruncated | RequestError::InvalidEnvelope(_) => "HTTP/1.1 400 Bad Request",
            RequestError::StorageFailure(_) => "HTTP/1.1 500 Internal Server Error",
        }
    }
}

/// Reads exactly the request headers (up to the blank line), enforcing
/// `MAX_HEADER_BYTES` against the *header's own length*, not the total bytes
/// read so far -- a single `read()` can return body bytes coalesced onto the
/// same TCP segment as the header terminator, so `buf.len()` alone would
/// wrongly reject a small, valid header just because a large body arrived
/// attached to it. Generic over `Read` so it can be unit-tested with an
/// in-memory reader instead of a real socket.
fn read_headers<R: Read>(stream: &mut R) -> Result<(Vec<u8>, usize), RequestError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let header_len = pos + 4;
            // The limit applies to the header itself -- bytes past the
            // terminator are body, already validated separately against
            // `MAX_BODY_BYTES` by the caller.
            if header_len > MAX_HEADER_BYTES {
                return Err(RequestError::HeadersTooLarge);
            }
            return Ok((buf, header_len));
        }
        // No terminator yet: only now is total buffered length a fair proxy
        // for "header too large" -- there's no body/header split to speak of
        // until the terminator actually shows up.
        if buf.len() > MAX_HEADER_BYTES {
            return Err(RequestError::HeadersTooLarge);
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(RequestError::ConnectionClosedEarly);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// Reads exactly `content_length` body bytes (any already read as part of
/// the initial header buffer are accounted for). Returns
/// `BodyTruncated` rather than silently accepting a short body if the
/// connection closes early -- the old Milestone 12 receiver returned `200
/// OK` even then.
fn read_exact_body(
    stream: &mut TcpStream,
    mut buf: Vec<u8>,
    headers_end: usize,
    content_length: usize,
) -> Result<Vec<u8>, RequestError> {
    let mut chunk = [0u8; 4096];
    while buf.len() < headers_end + content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(RequestError::BodyTruncated);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    buf.truncate(headers_end + content_length);
    Ok(buf.split_off(headers_end))
}

/// Writes `payload` to a private temp file, then atomically "commits" it to
/// a collision-safe final name (`event_<unix_millis>_<label>.<ext>`, or with
/// a `_<N>` suffix on collision) via `hard_link` rather than `rename`.
///
/// This distinction matters: `rename` on POSIX silently replaces an
/// existing destination, so retrying it on a "taken" name can't detect the
/// collision at all. `hard_link` fails with `AlreadyExists` instead, which
/// is what actually lets us retry with the next suffix safely -- including
/// long after an earlier event's temp file for the same millisecond+label
/// has already been cleaned up (so re-trying `create_new` on a *new* temp
/// name would never notice the final name was taken).
///
/// `sync_all` (not `flush`, which is a no-op for `std::fs::File` -- it does
/// not force the OS to write data to disk) is called before the payload is
/// ever exposed under a final name, so `payload` is actually durable against
/// a crash or power loss by the time a reader could observe it.
///
/// Once `hard_link` succeeds, `payload` is fully written and durable under
/// `final_path` -- that return value is the function's actual success
/// signal. Everything after it (removing the now-redundant temp name) is
/// just tidiness: if it fails, the temp file is harmless untracked litter,
/// not a correctness problem, so it must not turn a real success into a
/// reported failure (which would hide the committed file from
/// `store_event`'s rollback bookkeeping -- a partial event that survives
/// forever instead of one that gets cleanly rolled back).
fn commit_part_file(
    dir: &str,
    timestamp: u128,
    label: &str,
    ext: &str,
    payload: &[u8],
) -> std::io::Result<String> {
    let mut tmp_suffix = 0u32;
    let (tmp_path, mut tmp_file) = loop {
        let candidate = format!("{dir}/.tmp_{timestamp}_{label}_{tmp_suffix}");
        match OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(file) => break (candidate, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                tmp_suffix += 1;
                continue;
            }
            Err(e) => return Err(e),
        }
    };
    if let Err(e) = tmp_file.write_all(payload).and_then(|()| tmp_file.sync_all()) {
        drop(tmp_file);
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    drop(tmp_file);

    let mut final_suffix = 0u32;
    let final_path = loop {
        let candidate = if final_suffix == 0 {
            format!("{dir}/event_{timestamp}_{label}.{ext}")
        } else {
            format!("{dir}/event_{timestamp}_{label}_{final_suffix}.{ext}")
        };
        match fs::hard_link(&tmp_path, &candidate) {
            Ok(()) => break candidate,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                final_suffix += 1;
                continue;
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(e);
            }
        }
    };

    // The event is already fully durable and visible under `final_path` at
    // this point -- a failure removing the leftover temp name must not be
    // reported as this call failing.
    if let Err(e) = fs::remove_file(&tmp_path) {
        println!("warning: committed {final_path} but failed to remove temp file {tmp_path}: {e}");
    }
    Ok(final_path)
}

/// The on-disk label and file extension for a given part kind. Thumbnail
/// parts are real JPEG bytes, so `.jpg` makes them directly openable;
/// video/audio containers aren't decided yet (see `PROJECT_STATUS.md`), so
/// they land as `.bin` until that's designed.
fn part_file_naming(kind: PartKind) -> (&'static str, &'static str) {
    match kind {
        PartKind::Thumbnail => ("thumbnail", "jpg"),
        PartKind::Video => ("video", "bin"),
        PartKind::Audio => ("audio", "bin"),
    }
}

/// Tracks recently seen event IDs so a retried upload (firmware resending
/// the same event because it never got a confirmed response) doesn't get
/// stored a second time under a new receipt timestamp -- firmware's only
/// identity for an event is `event_id`, chosen once and reused across
/// retries, so this is the only place that can catch the duplicate.
///
/// Deliberately bounded and FIFO, not a permanent log: this only needs to
/// cover retries arriving reasonably close together, not the server's
/// entire lifetime, so a fixed-size ring is enough -- no database. If
/// constructed via `new_with_persistence`, the ring is also mirrored to a
/// small file on disk (see `persist_path`), so a firmware retry landing
/// right after a server restart is still recognized instead of being
/// re-stored -- this is exactly the gap Milestone 16 flagged as a known
/// limitation (dedup forgetting everything across a restart), which the
/// SD-backed firmware queue (Milestone 17) now makes a real possibility:
/// firmware may replay a queued event well after the server that already
/// stored it has since restarted.
struct EventDedup {
    seen: std::collections::VecDeque<u64>,
    capacity: usize,
    persist_path: Option<std::path::PathBuf>,
}

impl EventDedup {
    /// In-memory only, no persistence -- used by tests, which want a plain
    /// `EventDedup` without touching the filesystem. Production code always
    /// goes through `new_with_persistence` instead.
    #[cfg(test)]
    fn new(capacity: usize) -> Self {
        Self {
            seen: std::collections::VecDeque::with_capacity(capacity),
            capacity,
            persist_path: None,
        }
    }

    /// Like `new`, but loads any previously recorded event IDs from `path`
    /// (one decimal number per line) if it already exists, and keeps `path`
    /// up to date on every subsequent `record` -- so dedup survives a
    /// server restart, not just retries within one run. A missing or
    /// unreadable file is treated the same as an empty one (nothing to
    /// load yet, e.g. first run) rather than a startup failure; a
    /// corrupted line is just skipped.
    fn new_with_persistence(capacity: usize, path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        let mut seen = std::collections::VecDeque::with_capacity(capacity);
        if let Ok(contents) = fs::read_to_string(&path) {
            for line in contents.lines() {
                if let Ok(id) = line.trim().parse::<u64>() {
                    if seen.len() >= capacity {
                        seen.pop_front();
                    }
                    seen.push_back(id);
                }
            }
        }
        Self {
            seen,
            capacity,
            persist_path: Some(path),
        }
    }

    /// Returns `true` if `event_id` has already been successfully stored.
    /// Read-only -- does *not* record anything, so a request that turns out
    /// to be invalid or fails to store doesn't burn the ID for a later,
    /// corrected retry. Call `record` only once the event has actually been
    /// committed to disk.
    fn is_duplicate(&self, event_id: u64) -> bool {
        self.seen.contains(&event_id)
    }

    /// Records `event_id` as successfully stored. Must only be called after
    /// the event is fully committed -- recording an ID that didn't actually
    /// get stored (e.g. because parsing or storage failed) would make a
    /// legitimate retry look like a duplicate and silently return `200`
    /// without ever writing anything.
    fn record(&mut self, event_id: u64) {
        if self.seen.contains(&event_id) {
            return;
        }
        if self.seen.len() >= self.capacity {
            self.seen.pop_front();
        }
        self.seen.push_back(event_id);

        if let Some(path) = &self.persist_path
            && let Err(e) = Self::persist(path, &self.seen)
        {
            println!("warning: failed to persist event dedup state to {path:?}: {e}");
        }
    }

    /// Rewrites the persisted dedup file to exactly match `seen` -- a full
    /// rewrite rather than an ever-growing append log, since `seen` is
    /// already bounded to `capacity` entries (a few KB at most). Written to
    /// a temp file and renamed into place, the same crash-safe pattern
    /// `commit_part_file` uses, so a crash mid-write leaves the previous,
    /// still-valid version in place rather than a half-written one.
    fn persist(path: &std::path::Path, seen: &std::collections::VecDeque<u64>) -> std::io::Result<()> {
        let mut contents = String::new();
        for id in seen {
            contents.push_str(&id.to_string());
            contents.push('\n');
        }
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, contents)?;
        fs::rename(&tmp_path, path)
    }
}

/// Parses the `shared` crate's event envelope out of `body` and writes each
/// part to its own file under `dir` via `committer`, all sharing one
/// timestamp so parts from the same event sort together. Returns the stored
/// filenames in part order, for logging -- empty if `event_id` is a
/// duplicate of one already seen by `dedup` (nothing new was written, but
/// this is still success from the caller's perspective: the event is, in
/// fact, already stored).
///
/// The whole envelope is parsed -- and thus fully validated -- before
/// anything is written to disk, so a malformed part N can never leave parts
/// 1..N-1 behind as an orphaned partial event. If a part still fails to
/// *store* after that (disk full, permissions, ...), every part already
/// committed for this event is rolled back before returning the error, so
/// callers never observe a partial event under real filenames either way.
///
/// `committer` is a parameter (rather than calling `commit_part_file`
/// directly) purely for testability: real storage failures only manifest
/// deterministically at the directory level (permissions, quota), which
/// can't be scoped to "only the second part" from outside this function --
/// any pre-existing file/dir at a candidate path is, correctly, just
/// treated as a name collision and retried, not a hard failure. Injecting a
/// fake committer lets tests simulate "part N specifically fails" to verify
/// the rollback logic actually rolls back the *right* parts, not just that
/// zero-vs-nonzero parts got written.
fn store_event_with_committer(
    dir: &str,
    body: &[u8],
    dedup: &mut EventDedup,
    mut committer: impl FnMut(&str, u128, &str, &str, &[u8]) -> std::io::Result<String>,
) -> Result<Vec<String>, RequestError> {
    let (header, rest) = decode_envelope_header(body).map_err(RequestError::InvalidEnvelope)?;

    if dedup.is_duplicate(header.event_id) {
        println!(
            "  event_id={} already stored -- treating as a retry, not writing again",
            header.event_id
        );
        return Ok(Vec::new());
    }

    let parts: Vec<(PartHeader, &[u8])> = PartsIter::new(rest, header.part_count)
        .collect::<Result<_, _>>()
        .map_err(RequestError::InvalidEnvelope)?;

    fs::create_dir_all(dir).map_err(RequestError::StorageFailure)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let mut filenames = Vec::new();
    for (part_header, payload) in parts {
        let (label, ext) = part_file_naming(part_header.kind);
        println!(
            "  event_id={} part: kind={:?} encoding={:?} timestamp_ms={} duration_ms={} len={}",
            header.event_id,
            part_header.kind,
            part_header.encoding,
            part_header.timestamp_ms,
            part_header.duration_ms,
            part_header.len
        );
        match committer(dir, timestamp, label, ext, payload) {
            Ok(filename) => filenames.push(filename),
            Err(e) => {
                for filename in &filenames {
                    let _ = fs::remove_file(filename);
                }
                return Err(RequestError::StorageFailure(e));
            }
        }
    }

    // Only recorded once every part is actually committed -- a request that
    // fails validation or storage must not burn this event_id, or a later
    // retry with the same ID (possibly corrected, possibly just luckier
    // with disk space) would be wrongly treated as an already-stored
    // duplicate and get a 200 without ever being written.
    dedup.record(header.event_id);
    Ok(filenames)
}

fn store_event(dir: &str, body: &[u8], dedup: &mut EventDedup) -> Result<Vec<String>, RequestError> {
    store_event_with_committer(dir, body, dedup, commit_part_file)
}

/// Does the actual request handling; the caller is responsible for turning
/// an `Err` into the right HTTP error response on the same stream.
fn process_request(
    stream: &mut TcpStream,
    dedup: &mut EventDedup,
    analyzer: &Arc<dyn ai::ThumbnailAnalyzer>,
) -> Result<(), RequestError> {
    let peer = stream.peer_addr()?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    println!("connection from {peer}");

    let (buf, headers_end) = read_headers(stream)?;
    let headers = String::from_utf8_lossy(&buf[..headers_end]).into_owned();

    let request_line = headers.lines().next().unwrap_or("");
    if request_line != "POST /upload HTTP/1.1" {
        println!("rejected: bad request line {request_line:?}");
        return Err(RequestError::BadRequestLine);
    }

    let token = header_value(&headers, "X-Upload-Token");
    if token != Some(config::UPLOAD_TOKEN) {
        println!("rejected: missing/invalid X-Upload-Token");
        return Err(RequestError::Unauthorized);
    }

    let content_length: usize = header_value(&headers, "Content-Length")
        .ok_or(RequestError::MissingContentLength)?
        .parse()
        .map_err(|_| RequestError::MissingContentLength)?;

    if content_length > MAX_BODY_BYTES {
        println!("rejected: body {content_length} bytes exceeds max {MAX_BODY_BYTES}");
        return Err(RequestError::BodyTooLarge);
    }

    let body = read_exact_body(stream, buf, headers_end, content_length)?;
    println!("body: {} bytes, validated complete", body.len());

    let filenames = store_event(UPLOADS_DIR, &body, dedup)?;
    for filename in &filenames {
        println!("stored: {filename}");
    }

    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(response)?;
    stream.flush()?;

    // Strictly after the response above, and on its own detached thread --
    // firmware's upload has already succeeded or failed on its own merits
    // by this point, and this server's accept loop is single-threaded, so
    // an in-line call here would block every subsequent upload from even
    // being accepted until Ollama finished. A duplicate event (empty
    // `filenames`, nothing new stored) has nothing new to analyze either.
    if let Some(thumbnail_path) = filenames.iter().find(|f| f.ends_with("_thumbnail.jpg")) {
        let analysis_path = format!("{}_analysis.json", thumbnail_path.trim_end_matches("_thumbnail.jpg"));
        let thumbnail_path = thumbnail_path.clone();
        let analyzer = Arc::clone(analyzer);
        std::thread::spawn(move || {
            ai::analyze_and_save(analyzer.as_ref(), Path::new(&thumbnail_path), Path::new(&analysis_path));
        });
    }

    Ok(())
}

fn handle_connection(mut stream: TcpStream, dedup: &mut EventDedup, analyzer: &Arc<dyn ai::ThumbnailAnalyzer>) {
    if let Err(e) = process_request(&mut stream, dedup, analyzer) {
        println!("request failed: {e:?}");
        let _ = stream.write_all(e.status_line().as_bytes());
        let _ = stream.write_all(b"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = stream.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Builds a byte buffer of exactly `total_len` bytes ending in the
    /// header terminator `\r\n\r\n` -- `read_headers` only looks for that
    /// exact byte sequence, so the padding before it doesn't need to be
    /// real HTTP syntax to exercise its boundary logic.
    fn header_of_len(total_len: usize) -> Vec<u8> {
        assert!(total_len >= 4);
        let mut data = vec![b'X'; total_len - 4];
        data.extend_from_slice(b"\r\n\r\n");
        data
    }

    #[test]
    fn header_exactly_at_limit_is_accepted() {
        let data = header_of_len(MAX_HEADER_BYTES);
        let mut reader = Cursor::new(data);
        let (_buf, header_len) = read_headers(&mut reader).expect("exact-limit header should be accepted");
        assert_eq!(header_len, MAX_HEADER_BYTES);
    }

    #[test]
    fn header_one_byte_over_limit_is_rejected() {
        let data = header_of_len(MAX_HEADER_BYTES + 1);
        let mut reader = Cursor::new(data);
        let err = read_headers(&mut reader).expect_err("over-limit header should be rejected");
        assert!(matches!(err, RequestError::HeadersTooLarge));
    }

    #[test]
    fn header_under_limit_with_coalesced_body_reaching_limit_is_accepted() {
        // The actual bug this test guards against: the header's terminator
        // can be discovered in a read whose *cumulative* buffer (header +
        // whatever body happened to arrive in the same underlying reads)
        // reaches MAX_HEADER_BYTES, even though the header itself is well
        // under it. Only header_len must be checked, not buf.len().
        //
        // read_headers() pulls a fixed 4096-byte chunk per call, so with an
        // 8192-byte limit (exactly two chunks) the terminator-discovery
        // read can never leave buf.len() *past* the limit while header_len
        // stays under it -- that would require the terminator itself to sit
        // beyond byte 8192, which would make header_len exceed the limit
        // too. The strongest reachable case is buf.len() landing exactly AT
        // the limit while header_len sits comfortably below it, which is
        // exactly as damning for a buggy `buf.len()`-based check: a literal
        // `buf.len() >= MAX_HEADER_BYTES` bug would reject this header.
        let header_len_expected = 8_000; // comfortably under the 8192 limit
        let mut data = header_of_len(header_len_expected);
        // Trailing body, coalesced into the same underlying reads, large
        // enough that the second 4096-byte read is a full one and lands
        // buf.len() exactly on MAX_HEADER_BYTES (see comment above for why
        // it can't land past it).
        data.extend(vec![b'B'; 4096]);
        let mut reader = Cursor::new(data);

        let (buf, header_len) =
            read_headers(&mut reader).expect("header under the limit with a coalesced body should be accepted");
        assert_eq!(header_len, header_len_expected);
        assert_eq!(buf.len(), MAX_HEADER_BYTES);
    }

    #[test]
    fn missing_terminator_past_limit_is_rejected() {
        // No `\r\n\r\n` anywhere -- an unterminated header that just keeps
        // growing must still be rejected once it exceeds the limit.
        let data = vec![b'X'; MAX_HEADER_BYTES + 1];
        let mut reader = Cursor::new(data);
        let err = read_headers(&mut reader).expect_err("unterminated over-limit data should be rejected");
        assert!(matches!(err, RequestError::HeadersTooLarge));
    }

    /// A scratch directory under the OS temp dir, unique per test run, so
    /// concurrent `cargo test` runs (and repeated runs) never collide or
    /// interfere with the real `UPLOADS_DIR`. Callers remove it when done.
    fn scratch_dir(label: &str) -> String {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("camera_server_test_{label}_{unique}"))
            .to_string_lossy()
            .into_owned()
    }

    fn thumbnail_part_header(len: u32) -> [u8; shared::PART_HEADER_LEN] {
        shared::encode_part_header(PartKind::Thumbnail, shared::Encoding::Jpeg, 0, 0, len)
    }

    fn video_part_header(len: u32) -> [u8; shared::PART_HEADER_LEN] {
        shared::encode_part_header(PartKind::Video, shared::Encoding::RecorderFrames, 0, 5000, len)
    }

    #[test]
    fn store_event_writes_a_thumbnail_part_as_jpg() {
        let dir = scratch_dir("thumbnail");
        let jpeg = b"fake-jpeg-bytes";
        let mut body = Vec::new();
        body.extend_from_slice(&shared::encode_envelope_header(1, 1));
        body.extend_from_slice(&thumbnail_part_header(jpeg.len() as u32));
        body.extend_from_slice(jpeg);

        let mut dedup = EventDedup::new(16);
        let filenames = store_event(&dir, &body, &mut dedup).expect("valid single-thumbnail event should store");
        assert_eq!(filenames.len(), 1);
        assert!(filenames[0].ends_with(".jpg"), "got {:?}", filenames[0]);
        let stored = fs::read(&filenames[0]).expect("stored file should be readable");
        assert_eq!(stored, jpeg);
        // No temp files should be left behind once the event is fully
        // committed.
        let leftover_tmp = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().starts_with(".tmp_"));
        assert!(!leftover_tmp, "temp file left behind after successful commit");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn event_dedup_detects_repeats_and_evicts_oldest_past_capacity() {
        let mut dedup = EventDedup::new(2);
        assert!(!dedup.is_duplicate(1), "1 is new");
        dedup.record(1);
        assert!(!dedup.is_duplicate(2), "2 is new");
        dedup.record(2);
        assert!(dedup.is_duplicate(1), "1 was already recorded");
        // Pushing a 3rd past capacity 2 evicts the oldest (1) -- documents
        // the bounded/FIFO tradeoff: this only needs to catch retries close
        // together in time, not the server's entire run.
        assert!(!dedup.is_duplicate(3), "3 is new");
        dedup.record(3);
        assert!(!dedup.is_duplicate(1), "1 was evicted, looks new again");
    }

    #[test]
    fn event_dedup_does_not_record_until_told_to() {
        // Checking is_duplicate must never itself mark an ID as seen --
        // only `record` does. This is the property the whole retry-safety
        // fix depends on: a failed attempt must not burn the event_id.
        let dedup = EventDedup::new(16);
        assert!(!dedup.is_duplicate(1));
        assert!(!dedup.is_duplicate(1), "repeated checks alone must not record");
    }

    fn scratch_file(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("camera_server_test_dedup_{label}_{unique}.log"))
    }

    #[test]
    fn persistent_dedup_survives_a_simulated_restart() {
        let path = scratch_file("restart");

        let mut dedup = EventDedup::new_with_persistence(16, &path);
        assert!(!dedup.is_duplicate(42));
        dedup.record(42);
        drop(dedup);

        // A fresh instance pointed at the same path is a stand-in for the
        // server process restarting -- the whole point of persistence is
        // that this doesn't forget what the first instance already knew.
        let dedup_after_restart = EventDedup::new_with_persistence(16, &path);
        assert!(dedup_after_restart.is_duplicate(42), "restart must not forget a recorded event_id");
        assert!(!dedup_after_restart.is_duplicate(99));

        fs::remove_file(&path).ok();
    }

    #[test]
    fn persistent_dedup_starts_empty_when_no_file_exists_yet() {
        let path = scratch_file("missing");
        fs::remove_file(&path).ok(); // guarantee it doesn't exist

        let dedup = EventDedup::new_with_persistence(16, &path);
        assert!(!dedup.is_duplicate(1), "no prior file means nothing has been seen yet");
    }

    #[test]
    fn persistent_dedup_respects_capacity_across_a_restart() {
        let path = scratch_file("capacity");

        let mut dedup = EventDedup::new_with_persistence(2, &path);
        dedup.record(1);
        dedup.record(2);
        dedup.record(3); // evicts 1 in memory, and in the persisted file too
        drop(dedup);

        let dedup_after_restart = EventDedup::new_with_persistence(2, &path);
        assert!(
            !dedup_after_restart.is_duplicate(1),
            "1 was evicted before the restart, must not reappear after loading"
        );
        assert!(dedup_after_restart.is_duplicate(2));
        assert!(dedup_after_restart.is_duplicate(3));

        fs::remove_file(&path).ok();
    }

    #[test]
    fn persistent_dedup_ignores_a_corrupted_line_instead_of_failing_to_start() {
        let path = scratch_file("corrupt");
        fs::write(&path, "7\nnot-a-number\n8\n").expect("scratch file should be writable");

        let dedup = EventDedup::new_with_persistence(16, &path);
        assert!(dedup.is_duplicate(7));
        assert!(dedup.is_duplicate(8));

        fs::remove_file(&path).ok();
    }

    #[test]
    fn store_event_skips_storing_a_duplicate_event_id() {
        let dir = scratch_dir("dedup");
        let jpeg = b"fake-jpeg-bytes";
        let mut body = Vec::new();
        body.extend_from_slice(&shared::encode_envelope_header(1, 42));
        body.extend_from_slice(&thumbnail_part_header(jpeg.len() as u32));
        body.extend_from_slice(jpeg);

        let mut dedup = EventDedup::new(16);
        let first = store_event(&dir, &body, &mut dedup).expect("first upload of this event should store");
        assert_eq!(first.len(), 1);

        let second =
            store_event(&dir, &body, &mut dedup).expect("retry of the same event_id should not error");
        assert!(second.is_empty(), "retry should not create new files, got {second:?}");

        let files: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(files.len(), 1, "expected exactly one file, found {files:?}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_event_allows_retry_after_a_storage_failure() {
        // Regression test for the dedup-timing bug: recording event_id as
        // seen *before* storage actually succeeds would make this retry
        // wrongly look like an already-stored duplicate and silently
        // return success without ever writing the file.
        let dir = scratch_dir("retry_after_storage_failure");
        let jpeg = b"fake-jpeg-bytes";
        let mut body = Vec::new();
        body.extend_from_slice(&shared::encode_envelope_header(1, 77));
        body.extend_from_slice(&thumbnail_part_header(jpeg.len() as u32));
        body.extend_from_slice(jpeg);

        let mut dedup = EventDedup::new(16);

        // First attempt: every part fails to commit (simulating a
        // transient disk error) -- must fail, and must not mark event_id
        // 77 as already stored.
        let first = store_event_with_committer(&dir, &body, &mut dedup, |_, _, _, _, _| {
            Err(std::io::Error::other("simulated transient storage failure"))
        });
        assert!(matches!(first, Err(RequestError::StorageFailure(_))));
        assert!(!dedup.is_duplicate(77), "a failed attempt must not burn the event_id");

        // Retry with the same event_id and the real committer -- must
        // actually store this time, not be skipped as a duplicate.
        let second = store_event(&dir, &body, &mut dedup).expect("retry after a storage failure should store");
        assert_eq!(second.len(), 1, "retry should actually write the file this time");
        assert!(second[0].ends_with(".jpg"));
        assert_eq!(fs::read(&second[0]).unwrap(), jpeg);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_event_allows_retry_after_invalid_payload_with_same_event_id() {
        // Same regression as above, but for a malformed *request* rather
        // than a storage failure -- the envelope header (and thus
        // event_id) parses fine, but the declared part length doesn't
        // match what's actually present.
        let dir = scratch_dir("retry_after_invalid_payload");
        let jpeg = b"fake-jpeg-bytes";

        let mut bad_body = Vec::new();
        bad_body.extend_from_slice(&shared::encode_envelope_header(1, 88));
        bad_body.extend_from_slice(&thumbnail_part_header(100)); // claims 100 bytes
        bad_body.extend_from_slice(b"only 5"); // far fewer actually present

        let mut dedup = EventDedup::new(16);
        let first = store_event(&dir, &bad_body, &mut dedup);
        assert!(matches!(
            first,
            Err(RequestError::InvalidEnvelope(EnvelopeError::PartLengthExceedsBuffer))
        ));
        assert!(!dedup.is_duplicate(88), "an invalid request must not burn the event_id");

        // Retry with the same event_id and a corrected, well-formed body --
        // must actually store.
        let mut good_body = Vec::new();
        good_body.extend_from_slice(&shared::encode_envelope_header(1, 88));
        good_body.extend_from_slice(&thumbnail_part_header(jpeg.len() as u32));
        good_body.extend_from_slice(jpeg);

        let second = store_event(&dir, &good_body, &mut dedup)
            .expect("retry with a corrected payload and the same event_id should store");
        assert_eq!(second.len(), 1);
        assert_eq!(fs::read(&second[0]).unwrap(), jpeg);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_event_writes_one_file_per_part() {
        let dir = scratch_dir("multipart");
        let thumb = b"thumb-bytes";
        let video = b"video-bytes";
        let mut body = Vec::new();
        body.extend_from_slice(&shared::encode_envelope_header(2, 2));
        body.extend_from_slice(&thumbnail_part_header(thumb.len() as u32));
        body.extend_from_slice(thumb);
        body.extend_from_slice(&video_part_header(video.len() as u32));
        body.extend_from_slice(video);

        let mut dedup = EventDedup::new(16);
        let filenames = store_event(&dir, &body, &mut dedup).expect("valid two-part event should store");
        assert_eq!(filenames.len(), 2);
        assert!(filenames[0].ends_with("_thumbnail.jpg"), "got {:?}", filenames[0]);
        assert!(filenames[1].ends_with("_video.bin"), "got {:?}", filenames[1]);
        assert_eq!(fs::read(&filenames[0]).unwrap(), thumb);
        assert_eq!(fs::read(&filenames[1]).unwrap(), video);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_event_rejects_malformed_envelope() {
        let dir = scratch_dir("malformed");
        let mut dedup = EventDedup::new(16);
        let err = store_event(&dir, b"not an envelope", &mut dedup).expect_err("garbage body should be rejected");
        assert!(matches!(
            err,
            RequestError::InvalidEnvelope(EnvelopeError::BadMagic)
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_event_rejects_zero_part_envelope() {
        let dir = scratch_dir("zero_parts");
        let body = shared::encode_envelope_header(0, 1);
        let mut dedup = EventDedup::new(16);
        let err = store_event(&dir, &body, &mut dedup).expect_err("zero-part envelope should be rejected");
        assert!(matches!(
            err,
            RequestError::InvalidEnvelope(EnvelopeError::EmptyEvent)
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_event_rejects_trailing_bytes_and_writes_nothing() {
        let dir = scratch_dir("trailing_bytes");
        let jpeg = b"fake-jpeg-bytes";
        let mut body = Vec::new();
        body.extend_from_slice(&shared::encode_envelope_header(1, 1));
        body.extend_from_slice(&thumbnail_part_header(jpeg.len() as u32));
        body.extend_from_slice(jpeg);
        body.extend_from_slice(b"trailing-junk-not-declared-by-part-count");

        let mut dedup = EventDedup::new(16);
        let err = store_event(&dir, &body, &mut dedup).expect_err("trailing bytes should be rejected");
        assert!(matches!(
            err,
            RequestError::InvalidEnvelope(EnvelopeError::TrailingBytes)
        ));
        // Parsing fails before any part is written, so the directory should
        // never even get created.
        assert!(!std::path::Path::new(&dir).exists());
    }

    #[test]
    fn store_event_writes_nothing_when_the_first_part_fails_to_commit() {
        let dir = scratch_dir("fail_first");
        let thumb = b"thumb-bytes";
        let video = b"video-bytes";
        let mut body = Vec::new();
        body.extend_from_slice(&shared::encode_envelope_header(2, 10));
        body.extend_from_slice(&thumbnail_part_header(thumb.len() as u32));
        body.extend_from_slice(thumb);
        body.extend_from_slice(&video_part_header(video.len() as u32));
        body.extend_from_slice(video);

        let mut dedup = EventDedup::new(16);
        let err = store_event_with_committer(&dir, &body, &mut dedup, |_, _, _, _, _| {
            Err(std::io::Error::other("simulated failure on first part"))
        })
        .expect_err("first part failing to commit should fail the whole event");
        assert!(matches!(err, RequestError::StorageFailure(_)));

        let leftover: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        assert!(leftover.is_empty(), "expected nothing written, found {leftover:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_event_rolls_back_the_first_part_when_the_second_part_fails_to_commit() {
        let dir = scratch_dir("fail_second");
        let thumb = b"thumb-bytes";
        let video = b"video-bytes";
        let mut body = Vec::new();
        body.extend_from_slice(&shared::encode_envelope_header(2, 11));
        body.extend_from_slice(&thumbnail_part_header(thumb.len() as u32));
        body.extend_from_slice(thumb);
        body.extend_from_slice(&video_part_header(video.len() as u32));
        body.extend_from_slice(video);

        let mut call_count = 0u32;
        let mut dedup = EventDedup::new(16);
        let err = store_event_with_committer(&dir, &body, &mut dedup, |dir, ts, label, ext, payload| {
            call_count += 1;
            if call_count == 1 {
                // Let the first part (thumbnail) really commit, so there is
                // a real file on disk to verify gets rolled back.
                commit_part_file(dir, ts, label, ext, payload)
            } else {
                Err(std::io::Error::other("simulated failure on second part"))
            }
        })
        .expect_err("second part failing to commit should fail the whole event");
        assert!(matches!(err, RequestError::StorageFailure(_)));
        assert_eq!(call_count, 2, "both parts should have been attempted");

        let leftover: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        assert!(
            leftover.is_empty(),
            "expected the first part's file to be rolled back, found {leftover:?}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_event_leaves_no_partial_files_when_storage_fails() {
        // A real (not simulated) directory-wide storage failure, exercising
        // the actual filesystem/permissions path rather than an injected
        // committer -- complements the two tests above, which pin down
        // *which* part failed and *which* files got rolled back precisely.
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir("rollback");
        fs::create_dir_all(&dir).unwrap();

        let thumb = b"thumb-bytes";
        let video = b"video-bytes";
        let mut body = Vec::new();
        body.extend_from_slice(&shared::encode_envelope_header(2, 3));
        body.extend_from_slice(&thumbnail_part_header(thumb.len() as u32));
        body.extend_from_slice(thumb);
        body.extend_from_slice(&video_part_header(video.len() as u32));
        body.extend_from_slice(video);

        // Read-only *after* creating the directory, so every part -- not
        // just a later one -- has nowhere to go: the simplest deterministic
        // way to force a real storage failure without depending on actual
        // disk space.
        let original_perms = fs::metadata(&dir).unwrap().permissions();
        let mut readonly = original_perms.clone();
        readonly.set_mode(0o500); // r-x------, no write
        fs::set_permissions(&dir, readonly).unwrap();

        let mut dedup = EventDedup::new(16);
        let result = store_event(&dir, &body, &mut dedup);

        // Restore permissions before asserting/cleaning up, so the test
        // doesn't leave an unremovable directory behind on failure.
        fs::set_permissions(&dir, original_perms).unwrap();

        let err = result.expect_err("write into a read-only directory should fail");
        assert!(matches!(err, RequestError::StorageFailure(_)));
        let leftover: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        assert!(leftover.is_empty(), "expected no files left behind, found {leftover:?}");

        fs::remove_dir_all(&dir).ok();
    }
}

/// How many recent event IDs to remember for retry deduplication -- covers
/// any realistic burst of retries for recent events without growing
/// unbounded; see `EventDedup`.
const EVENT_DEDUP_CAPACITY: usize = 256;
/// Sibling to `UPLOADS_DIR`, not inside it -- this is dedup bookkeeping, not
/// event media, and shouldn't show up mixed in with stored thumbnails/video.
const EVENT_DEDUP_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/event_dedup.log");
/// How often the retention sweep re-checks `UPLOADS_DIR` -- cheap (just a
/// directory listing + stat calls), so an hourly cadence is far more
/// granular than needed for a day-scale retention window while still
/// keeping old events from lingering long after they expire.
const RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", PORT))?;
    println!("listening on 0.0.0.0:{PORT} (trusted-LAN dev receiver only, see PROJECT_STATUS.md)");

    let mut dedup = EventDedup::new_with_persistence(EVENT_DEDUP_CAPACITY, EVENT_DEDUP_PATH);
    // Constructing this never talks to Ollama -- see `ai::OllamaAnalyzer`'s
    // doc comment. Startup must not depend on Ollama being up. `Arc` so each
    // event's background analysis thread (see `process_request`) can share
    // it without cloning the underlying config/HTTP setup per event.
    let analyzer: Arc<dyn ai::ThumbnailAnalyzer> = Arc::new(ai::OllamaAnalyzer::from_env(AI_ANALYSIS_TIMEOUT));

    // Runs independently of the accept loop below, for the same reason the
    // AI analysis call is never in-line: this server is single-threaded
    // and synchronous, so a sweep must never be able to delay accepting an
    // upload. Sweeps immediately on startup (catching anything that
    // expired while the server was down), then on `RETENTION_SWEEP_INTERVAL`.
    let retention_duration = retention::retention_from_env();
    println!(
        "retention: deleting event sets older than {:.1} day(s) (EVENT_RETENTION_DAYS)",
        retention_duration.as_secs_f64() / 86400.0
    );
    std::thread::spawn(move || loop {
        match retention::clean_expired_events(Path::new(UPLOADS_DIR), retention_duration, SystemTime::now()) {
            Ok(0) => {}
            Ok(n) => println!("retention: removed {n} expired event set(s)"),
            Err(e) => println!("retention: sweep failed: {e}"),
        }
        std::thread::sleep(RETENTION_SWEEP_INTERVAL);
    });

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => handle_connection(stream, &mut dedup, &analyzer),
            Err(e) => println!("accept error: {e}"),
        }
    }

    Ok(())
}
