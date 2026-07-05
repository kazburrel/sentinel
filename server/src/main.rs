//! Minimal-but-safe HTTP receiver for the ESP32 -> WiFi -> Mac upload path.
//!
//! Milestone 12 proved the pipe works at all; this hardens it per the
//! project's security review before any real camera footage gets wired in:
//! auth token, bounded header/body sizes, a read timeout, exact-body
//! validation (no 200 on a truncated upload), and safe server-chosen
//! filenames under one dedicated storage directory -- never a
//! client-supplied path. Still deliberately not a real router/framework;
//! just `POST /upload` with a token, nothing else.
//!
//! Only acceptable on a trusted LAN during development -- see
//! `PROJECT_STATUS.md`'s security threat model for what's still needed
//! before this is exposed beyond that (TLS, network segmentation).

mod config;

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
            RequestError::BodyTruncated => "HTTP/1.1 400 Bad Request",
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

/// Writes `body` under `UPLOADS_DIR` with a server-chosen filename,
/// guaranteed never to silently overwrite an existing file. A plain
/// `upload_<unix_millis>.bin` name isn't collision-proof on its own --
/// two uploads landing in the same millisecond would otherwise clobber each
/// other. `create_new(true)` atomically fails if the target already exists,
/// so on collision we just try the next suffix instead.
fn store_upload(body: &[u8]) -> std::io::Result<String> {
    fs::create_dir_all(UPLOADS_DIR)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let mut suffix = 0u32;
    loop {
        let filename = if suffix == 0 {
            format!("{UPLOADS_DIR}/upload_{timestamp}.bin")
        } else {
            format!("{UPLOADS_DIR}/upload_{timestamp}_{suffix}.bin")
        };

        match OpenOptions::new().write(true).create_new(true).open(&filename) {
            Ok(mut file) => {
                file.write_all(body)?;
                file.flush()?;
                return Ok(filename);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                suffix += 1;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Does the actual request handling; the caller is responsible for turning
/// an `Err` into the right HTTP error response on the same stream.
fn process_request(stream: &mut TcpStream) -> Result<(), RequestError> {
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

    let filename = store_upload(&body)?;
    println!("stored: {filename}");

    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(response)?;
    stream.flush()?;
    Ok(())
}

fn handle_connection(mut stream: TcpStream) {
    if let Err(e) = process_request(&mut stream) {
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
}

fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", PORT))?;
    println!("listening on 0.0.0.0:{PORT} (trusted-LAN dev receiver only, see PROJECT_STATUS.md)");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => handle_connection(stream),
            Err(e) => println!("accept error: {e}"),
        }
    }

    Ok(())
}
