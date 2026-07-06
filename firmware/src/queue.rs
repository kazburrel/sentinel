//! Persistent offline event queue: when `main.rs`'s in-memory upload retries
//! (`upload_event_with_retries`) are exhausted, the event's exact wire-format
//! envelope bytes are saved to a file on the onboard SD card instead of being
//! lost, so it can be retried after a later reconnect or a reboot. Uses the
//! same SDMMC/FAT stack Milestone 17's `sd_regression_test.rs` proved works
//! alongside camera/PIR/WiFi.
//!
//! Queued files hold exactly the bytes `upload_event` would have sent as the
//! POST body -- the `shared` crate's envelope header followed by each part's
//! header+payload -- so a queued file can be replayed to the server
//! byte-for-byte, just with the body streamed from SD instead of from
//! PSRAM/RAM. Nothing queue-specific is added to the wire format.
//!
//! Written to a fixed temp name first, then renamed to its final
//! `EVT_<event_id as 16 hex digits>.BIN` name only once fully written and
//! flushed -- a power loss mid-write can therefore never leave a
//! half-written file visible to the scan/retry pass, since it would still be
//! sitting under the temp name (or not exist at all).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use block_device_adapters::BufStream;
use block_device_driver::BlockDevice;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, Stack};
use embassy_time::{with_timeout, Duration};
use embedded_fatfs::{FileSystem, FsOptions, ReadWriteSeek};
use embedded_io_async::{Read as _, Write as _};
use embedded_partitions::mbr::Scheme;
use esp_println::println;
use shared::{encode_envelope_header, encode_part_header, Encoding, PartKind};

const QUEUE_TMP_NAME: &str = "EVT_TMP.BIN";
const QUEUE_PREFIX: &str = "EVT_";
const QUEUE_SUFFIX: &str = ".BIN";

/// Mirrors `main.rs`'s own connect/transfer timeouts (see its module doc) --
/// a queued-event replay must be bounded the same way a live upload is, so a
/// stalled server can't hang the drain pass forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub enum QueueError {
    /// The card has no MBR partition table, and none of its partitions (if
    /// any) hold a FAT filesystem, and it isn't a bare superfloppy FAT
    /// volume either -- nothing we can mount.
    NoFatVolume,
    /// Reading the partition table itself failed.
    PartitionScan,
    /// A FAT filesystem or file-level operation failed.
    Fs,
}

/// What happened replaying one queued file to the server -- deliberately
/// mirrors `main.rs`'s own `UploadOutcome` (kept as a separate type since
/// library code can't reference a type defined in a `[[bin]]` crate).
#[derive(Debug)]
enum ReplayOutcome {
    Success,
    /// The server responded, but not with `200` (parsed status code, `0` if
    /// unparseable).
    Rejected(u16),
    ConnectFailed,
    IoError,
    TimedOut,
}

impl ReplayOutcome {
    /// Whether this queued file is worth trying again later, or should just
    /// be dropped from the queue -- an unrecoverable `4xx` rejection would
    /// otherwise sit at the front of the queue forever, blocking every
    /// later file behind it from ever being attempted.
    fn is_retryable(&self) -> bool {
        match self {
            ReplayOutcome::Success => false,
            ReplayOutcome::Rejected(code) => *code >= 500,
            ReplayOutcome::ConnectFailed | ReplayOutcome::IoError | ReplayOutcome::TimedOut => true,
        }
    }
}

/// Result of one `drain_queue` pass.
#[derive(Debug)]
pub enum DrainOutcome {
    /// No queued files were present at all.
    Empty,
    /// Every queued file present at the start of the pass was uploaded (and
    /// deleted) or dropped as permanently unrecoverable.
    Drained { uploaded: u32, dropped: u32 },
    /// Stopped early: a file's replay came back as a transient failure (no
    /// connection, a timeout, an I/O error, or the server's own `5xx`),
    /// meaning the server or WiFi is most likely still down -- no point
    /// hammering through the rest of the queue this pass. Whatever's left
    /// (including the file that just failed) stays queued for next time.
    StoppedEarly { uploaded: u32, dropped: u32 },
}

fn queue_file_name(event_id: u64) -> String {
    format!("{QUEUE_PREFIX}{event_id:016X}{QUEUE_SUFFIX}")
}

/// Saves one event's envelope (thumbnail + video, exactly as
/// `upload_event` would send it) to the SD card as a new queued file.
#[allow(clippy::too_many_arguments)]
pub async fn save_event<BD: BlockDevice<512>>(
    card: &mut BD,
    event_id: u64,
    thumbnail: &[u8],
    video: &[u8],
    part_timestamp_ms: u32,
    video_duration_ms: u32,
) -> Result<(), QueueError> {
    let stream = BufStream::<_, 512>::new(card);
    match Scheme::open(stream).await {
        Ok(Scheme::Mbr(mut mbr)) => {
            let fat_idx = mbr.iter_used().find(|(_, p)| p.is_fat()).map(|(i, _)| i);
            match fat_idx {
                Some(idx) => match mbr.open_partition(idx).await {
                    Ok(part) => {
                        save_event_on(part, event_id, thumbnail, video, part_timestamp_ms, video_duration_ms).await
                    }
                    Err(_) => Err(QueueError::PartitionScan),
                },
                None => Err(QueueError::NoFatVolume),
            }
        }
        Ok(Scheme::Superfloppy(io)) => {
            save_event_on(io, event_id, thumbnail, video, part_timestamp_ms, video_duration_ms).await
        }
        Ok(Scheme::Unknown(_)) => Err(QueueError::NoFatVolume),
        Err(_) => Err(QueueError::PartitionScan),
    }
}

async fn save_event_on<IO: ReadWriteSeek>(
    io: IO,
    event_id: u64,
    thumbnail: &[u8],
    video: &[u8],
    part_timestamp_ms: u32,
    video_duration_ms: u32,
) -> Result<(), QueueError> {
    let fs = FileSystem::new(io, FsOptions::new()).await.map_err(|_| QueueError::Fs)?;

    let result: Result<(), QueueError> = async {
        let root = fs.root_dir();
        // A leftover temp file from a previous, power-cut-mid-write attempt
        // is harmless clutter, not a real event -- clear it before writing a
        // fresh one.
        let _ = root.remove(QUEUE_TMP_NAME).await;

        let mut f = root.create_file(QUEUE_TMP_NAME).await.map_err(|_| QueueError::Fs)?;
        f.truncate().await.map_err(|_| QueueError::Fs)?;

        let envelope_header = encode_envelope_header(2, event_id);
        let thumb_header = encode_part_header(
            PartKind::Thumbnail,
            Encoding::Jpeg,
            part_timestamp_ms,
            0,
            thumbnail.len() as u32,
        );
        let video_header = encode_part_header(
            PartKind::Video,
            Encoding::RecorderFrames,
            part_timestamp_ms,
            video_duration_ms,
            video.len() as u32,
        );
        f.write_all(&envelope_header).await.map_err(|_| QueueError::Fs)?;
        f.write_all(&thumb_header).await.map_err(|_| QueueError::Fs)?;
        f.write_all(thumbnail).await.map_err(|_| QueueError::Fs)?;
        f.write_all(&video_header).await.map_err(|_| QueueError::Fs)?;
        f.write_all(video).await.map_err(|_| QueueError::Fs)?;
        f.flush().await.map_err(|_| QueueError::Fs)?;
        drop(f);

        let final_name = queue_file_name(event_id);
        root.rename(QUEUE_TMP_NAME, &root, &final_name)
            .await
            .map_err(|_| QueueError::Fs)?;
        Ok(())
    }
    .await;

    // Unmount regardless of whether the save above succeeded -- a mount
    // left dangling on an error path is a worse outcome than an error we
    // already have in hand.
    let _ = fs.unmount().await;
    result
}

/// Uploads every currently-queued file to the server, deleting each one
/// once a `200` confirms the server has it, and dropping (without keeping)
/// any file whose replay comes back as a non-retryable rejection. Stops at
/// the first transient failure, leaving it and everything behind it queued
/// for a later attempt.
#[allow(clippy::too_many_arguments)]
pub async fn drain_queue<BD: BlockDevice<512>>(
    card: &mut BD,
    stack: Stack<'static>,
    rx_buf: &mut [u8],
    tx_buf: &mut [u8],
    scratch: &mut [u8],
    server_ip: [u8; 4],
    server_port: u16,
    upload_token: &str,
) -> Result<DrainOutcome, QueueError> {
    let stream = BufStream::<_, 512>::new(card);
    match Scheme::open(stream).await {
        Ok(Scheme::Mbr(mut mbr)) => {
            let fat_idx = mbr.iter_used().find(|(_, p)| p.is_fat()).map(|(i, _)| i);
            match fat_idx {
                Some(idx) => match mbr.open_partition(idx).await {
                    Ok(part) => {
                        drain_queue_on(part, stack, rx_buf, tx_buf, scratch, server_ip, server_port, upload_token)
                            .await
                    }
                    Err(_) => Err(QueueError::PartitionScan),
                },
                None => Err(QueueError::NoFatVolume),
            }
        }
        Ok(Scheme::Superfloppy(io)) => {
            drain_queue_on(io, stack, rx_buf, tx_buf, scratch, server_ip, server_port, upload_token).await
        }
        Ok(Scheme::Unknown(_)) => Err(QueueError::NoFatVolume),
        Err(_) => Err(QueueError::PartitionScan),
    }
}

#[allow(clippy::too_many_arguments)]
async fn drain_queue_on<IO: ReadWriteSeek>(
    io: IO,
    stack: Stack<'static>,
    rx_buf: &mut [u8],
    tx_buf: &mut [u8],
    scratch: &mut [u8],
    server_ip: [u8; 4],
    server_port: u16,
    upload_token: &str,
) -> Result<DrainOutcome, QueueError> {
    let fs = FileSystem::new(io, FsOptions::new()).await.map_err(|_| QueueError::Fs)?;

    let result = async {
        // Collect names+sizes first, rather than deleting files while an
        // iterator over the same directory is still live.
        let mut queued: Vec<(String, u64)> = Vec::new();
        {
            let root = fs.root_dir();
            let mut iter = root.iter();
            while let Some(entry) = iter.next().await {
                let entry = entry.map_err(|_| QueueError::Fs)?;
                if !entry.is_file() {
                    continue;
                }
                let name = entry.file_name();
                if name.starts_with(QUEUE_PREFIX) && name.ends_with(QUEUE_SUFFIX) && name != QUEUE_TMP_NAME {
                    queued.push((name, entry.len()));
                }
            }
        }

        if queued.is_empty() {
            return Ok(DrainOutcome::Empty);
        }
        println!("queue: {} event(s) waiting on SD, attempting redelivery...", queued.len());

        let mut uploaded = 0u32;
        let mut dropped = 0u32;
        for (name, len) in &queued {
            let root = fs.root_dir();
            let mut file = match root.open_file(name).await {
                Ok(f) => f,
                Err(_) => {
                    // Can't even open it -- drop it rather than get stuck
                    // retrying a file we can't read.
                    let _ = root.remove(name).await;
                    dropped += 1;
                    continue;
                }
            };

            let outcome = replay_file(
                &mut file,
                *len,
                stack,
                rx_buf,
                tx_buf,
                scratch,
                server_ip,
                server_port,
                upload_token,
            )
            .await;
            drop(file);

            match outcome {
                ReplayOutcome::Success => {
                    println!("queue: {name} delivered, removing from queue");
                    let _ = root.remove(name).await;
                    uploaded += 1;
                }
                other if !other.is_retryable() => {
                    println!("queue: {name} rejected permanently ({other:?}), dropping from queue");
                    let _ = root.remove(name).await;
                    dropped += 1;
                }
                other => {
                    println!("queue: {name} still undeliverable ({other:?}), stopping this pass");
                    return Ok(DrainOutcome::StoppedEarly { uploaded, dropped });
                }
            }
        }

        Ok(DrainOutcome::Drained { uploaded, dropped })
    }
    .await;

    let _ = fs.unmount().await;
    result
}

/// Streams one queued file's bytes as the POST body to the server -- the
/// file already holds a complete, valid envelope, so this only needs to
/// know its length (for `Content-Length`), never its internal structure.
#[allow(clippy::too_many_arguments)]
async fn replay_file<IO: ReadWriteSeek>(
    file: &mut embedded_fatfs::File<'_, IO, embedded_fatfs::DefaultTimeProvider, embedded_fatfs::LossyOemCpConverter>,
    len: u64,
    stack: Stack<'static>,
    rx_buf: &mut [u8],
    tx_buf: &mut [u8],
    scratch: &mut [u8],
    server_ip: [u8; 4],
    server_port: u16,
    upload_token: &str,
) -> ReplayOutcome {
    let request_head = format!(
        "POST /upload HTTP/1.1\r\n\
         Host: {}.{}.{}.{}:{server_port}\r\n\
         X-Upload-Token: {upload_token}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        server_ip[0], server_ip[1], server_ip[2], server_ip[3],
    );

    let ip = IpAddress::v4(server_ip[0], server_ip[1], server_ip[2], server_ip[3]);
    let mut socket = TcpSocket::new(stack, rx_buf, tx_buf);
    match with_timeout(CONNECT_TIMEOUT, socket.connect((ip, server_port))).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            socket.abort();
            return ReplayOutcome::ConnectFailed;
        }
        Err(_timeout) => {
            socket.abort();
            return ReplayOutcome::TimedOut;
        }
    }

    let mut resp_buf = [0u8; 512];
    let transfer_result = with_timeout(TRANSFER_TIMEOUT, async {
        socket.write_all(request_head.as_bytes()).await?;

        let mut remaining = len;
        while remaining > 0 {
            let want = core::cmp::min(scratch.len() as u64, remaining) as usize;
            let n = file
                .read(&mut scratch[..want])
                .await
                .map_err(|_| embassy_net::tcp::Error::ConnectionReset)?;
            if n == 0 {
                break;
            }
            socket.write_all(&scratch[..n]).await?;
            remaining -= n as u64;
        }

        socket.read(&mut resp_buf).await
    })
    .await;

    let n = match transfer_result {
        Ok(Ok(n)) => n,
        Ok(Err(_)) => {
            socket.abort();
            return ReplayOutcome::IoError;
        }
        Err(_timeout) => {
            socket.abort();
            return ReplayOutcome::TimedOut;
        }
    };

    let outcome = if n >= 12 && &resp_buf[..12] == b"HTTP/1.1 200" {
        ReplayOutcome::Success
    } else {
        let status_code = core::str::from_utf8(&resp_buf[..n])
            .ok()
            .and_then(|s| s.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        ReplayOutcome::Rejected(status_code)
    };
    socket.abort();
    outcome
}
