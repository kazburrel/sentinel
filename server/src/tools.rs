//! Rust-only command-line utilities used during camera development.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn run_if_requested(arguments: &[String]) -> Option<Result<(), String>> {
    let command = arguments.get(1)?.as_str();
    match command {
        "enroll-face" => Some(enroll_face(arguments)),
        "recognize-face" => Some(recognize_face(arguments)),
        "track-video" => Some(track_video(arguments)),
        "decode-capture" => Some(decode_capture(arguments)),
        "decode-raw" => Some(decode_raw(arguments)),
        "send-test-event" => Some(send_test_event(arguments)),
        _ => None,
    }
}

fn enroll_face(arguments: &[String]) -> Result<(), String> {
    if arguments.len() < 7 {
        return Err(
            "usage: server enroll-face <person-id> <display-name> <profile.json> <image-1> <image-2> [more images]"
                .to_string(),
        );
    }
    let images: Vec<PathBuf> = arguments[5..].iter().map(PathBuf::from).collect();
    let count = crate::identity::enroll_profile(
        &arguments[2],
        &arguments[3],
        Path::new(&arguments[4]),
        &images,
    )?;
    println!(
        "identity: enrolled {} as {} with {count} Rust-generated embedding(s)",
        arguments[2], arguments[3]
    );
    Ok(())
}

fn recognize_face(arguments: &[String]) -> Result<(), String> {
    if arguments.len() < 3 {
        return Err("usage: server recognize-face <image-1> [more images]".to_string());
    }
    let thumbnail = PathBuf::from(&arguments[2]);
    let keyframes: Vec<PathBuf> = arguments[3..].iter().map(PathBuf::from).collect();
    let result = crate::identity::recognize_event(&thumbnail, &keyframes);
    println!(
        "{}",
        serde_json::to_string(&result).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn track_video(arguments: &[String]) -> Result<(), String> {
    if arguments.len() < 4 {
        return Err(
            "usage: server track-video <event_video.mp4> <normal|minimal|threat> [--behavior] [--trusted]"
                .to_string(),
        );
    }
    let level = match arguments[3].as_str() {
        "normal" => crate::video::TrackerThreatLevel::Normal,
        "minimal" => crate::video::TrackerThreatLevel::Minimal,
        "threat" => crate::video::TrackerThreatLevel::Threat,
        _ => return Err("invalid tracker threat level".to_string()),
    };
    let path = crate::video::lock_mp4_with_tracker(
        Path::new(&arguments[2]),
        level,
        arguments.iter().any(|argument| argument == "--behavior"),
        arguments.iter().any(|argument| argument == "--trusted"),
    )
    .ok_or_else(|| "Rust video tracking failed".to_string())?;
    println!("{}", path.display());
    Ok(())
}

fn decode_capture(arguments: &[String]) -> Result<(), String> {
    if arguments.len() != 4 {
        return Err("usage: server decode-capture <log-file> <output-prefix>".to_string());
    }
    let contents = fs::read_to_string(&arguments[2]).map_err(|error| error.to_string())?;
    let mut frames = Vec::new();
    let mut chunks = String::new();
    let mut in_frame = false;
    for line in contents.lines().map(str::trim) {
        if line.starts_with("JPEG BEGIN") {
            in_frame = true;
            chunks.clear();
        } else if line == "JPEG END" {
            if in_frame {
                frames.push(decode_hex(&chunks)?);
            }
            in_frame = false;
        } else if in_frame {
            chunks.push_str(line);
        }
    }
    if frames.is_empty() {
        return Err("no JPEG frames found in log".to_string());
    }
    write_numbered_frames(
        Path::new(&arguments[3]),
        frames.into_iter().map(|frame| (0, frame)),
    )?;
    Ok(())
}

fn decode_raw(arguments: &[String]) -> Result<(), String> {
    if arguments.len() != 4 {
        return Err("usage: server decode-raw <log-file> <output-prefix>".to_string());
    }
    let data = fs::read(&arguments[2]).map_err(|error| error.to_string())?;
    let marker = b"RAW EXPORT BEGIN ";
    let marker_start =
        find_bytes(&data, marker).ok_or_else(|| "raw export marker not found".to_string())?;
    let line_end = data[marker_start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|offset| marker_start + offset)
        .ok_or_else(|| "unterminated raw export marker".to_string())?;
    let header =
        std::str::from_utf8(&data[marker_start..line_end]).map_err(|error| error.to_string())?;
    let expected: usize = header
        .split_whitespace()
        .next_back()
        .ok_or_else(|| "raw export byte count missing".to_string())?
        .parse::<usize>()
        .map_err(|error| error.to_string())?;
    let payload_start = line_end + 1;
    let payload_end = (payload_start + expected).min(data.len());
    let payload = &data[payload_start..payload_end];
    let mut offset = 0usize;
    let mut frames = Vec::new();
    while offset + 8 <= payload.len() {
        let length = u32::from_le_bytes(payload[offset..offset + 4].try_into().unwrap()) as usize;
        let timestamp = u32::from_le_bytes(payload[offset + 4..offset + 8].try_into().unwrap());
        offset += 8;
        if offset + length > payload.len() {
            break;
        }
        frames.push((timestamp, payload[offset..offset + length].to_vec()));
        offset += length;
    }
    if frames.is_empty() {
        return Err("no frames found in raw export".to_string());
    }
    if let (Some(first), Some(last)) = (frames.first(), frames.last()) {
        let elapsed = last.0.saturating_sub(first.0);
        let fps = if elapsed > 0 {
            (frames.len() - 1) as f64 / (elapsed as f64 / 1000.0)
        } else {
            0.0
        };
        println!("FPS {fps:.2}");
    }
    write_numbered_frames(Path::new(&arguments[3]), frames)?;
    Ok(())
}

fn send_test_event(arguments: &[String]) -> Result<(), String> {
    if !(3..=4).contains(&arguments.len()) {
        return Err("usage: server send-test-event <image.jpg> [server-ip]".to_string());
    }
    let jpeg_path = Path::new(&arguments[2]);
    let jpeg =
        fs::read(jpeg_path).map_err(|error| format!("read {}: {error}", jpeg_path.display()))?;
    let server_ip = arguments.get(3).map(String::as_str).unwrap_or("127.0.0.1");
    let sent_at = SystemTime::now();
    let event_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut body = Vec::new();
    body.extend_from_slice(&shared::encode_envelope_header(1, event_id));
    body.extend_from_slice(&shared::encode_part_header(
        shared::PartKind::Thumbnail,
        shared::Encoding::Jpeg,
        0,
        0,
        jpeg.len() as u32,
    ));
    body.extend_from_slice(&jpeg);
    let mut stream =
        TcpStream::connect((server_ip, crate::PORT)).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    write!(
        stream,
        "POST /upload HTTP/1.1\r\nHost: {server_ip}:{}\r\nX-Upload-Token: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        crate::PORT,
        crate::config::UPLOAD_TOKEN,
        body.len()
    )
    .map_err(|error| error.to_string())?;
    stream.write_all(&body).map_err(|error| error.to_string())?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| error.to_string())?;
    println!("{}", response.lines().next().unwrap_or("no response"));
    let deadline = SystemTime::now() + Duration::from_secs(35);
    while SystemTime::now() < deadline {
        if let Some(analysis) = newest_analysis_since(Path::new(crate::UPLOADS_DIR), sent_at) {
            println!(
                "{}",
                fs::read_to_string(&analysis).map_err(|error| error.to_string())?
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err("timed out waiting for analysis output".to_string())
}

fn newest_analysis_since(directory: &Path, since: SystemTime) -> Option<PathBuf> {
    fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let is_analysis = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_analysis.json"));
            let modified = entry.metadata().ok()?.modified().ok()?;
            (is_analysis && modified >= since).then_some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

fn write_numbered_frames(
    prefix: &Path,
    frames: impl IntoIterator<Item = (u32, Vec<u8>)>,
) -> Result<(), String> {
    let stem = prefix.with_extension("");
    for (index, (_, frame)) in frames.into_iter().enumerate() {
        let path = PathBuf::from(format!("{}_{index}.jpg", stem.display()));
        fs::write(&path, frame).map_err(|error| format!("write {}: {error}", path.display()))?;
        println!("{}", path.display());
    }
    Ok(())
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err("odd-length JPEG hex data".to_string());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digits = std::str::from_utf8(pair).map_err(|error| error.to_string())?;
            u8::from_str_radix(digits, 16).map_err(|error| error.to_string())
        })
        .collect()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_hex_decoder_accepts_bytes_and_rejects_odd_input() {
        assert_eq!(decode_hex("ffd80102").unwrap(), vec![0xff, 0xd8, 1, 2]);
        assert!(decode_hex("abc").is_err());
    }

    #[test]
    fn byte_marker_search_finds_the_first_match() {
        assert_eq!(
            find_bytes(b"xxRAW EXPORT BEGIN 4\n", b"RAW EXPORT BEGIN "),
            Some(2)
        );
        assert_eq!(find_bytes(b"nothing", b"RAW EXPORT BEGIN "), None);
    }
}
