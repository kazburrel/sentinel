//! Server-side key-frame extraction from a stored event's raw video part.
//!
//! Recorded clips are currently stored exactly as `firmware::recorder::
//! PsramRecorder` wrote them: `[frame_len: u32 LE][timestamp_ms: u32 LE]
//! [frame_len bytes of JPEG]` repeated (the `shared` crate's
//! `Encoding::RecorderFrames`), the same wire format
//! `scripts/decode_raw_capture.py` already parses for the USB export path
//! -- viewing one still requires this project's existing decode/assemble
//! tooling. This module doesn't do a full video conversion (no `ffmpeg`
//! dependency, no new external process, no new failure surface from a
//! process that might not be installed) -- it pulls out a handful of
//! representative JPEG stills so a future video-AI step (or a human) has
//! something immediately viewable/consumable, deliberately deferring a
//! full container conversion until it's clear one is actually needed.
//!
//! Runs after the event is already stored, on its own detached background
//! thread (see `main.rs`) -- exactly like `ai::analyze_and_save`: this
//! server's accept loop is single-threaded, so this must never run
//! in-line with request handling, and conversion success or failure must
//! never affect whether the upload itself succeeded. The original `.bin`
//! file is always kept regardless -- nothing here ever deletes or
//! modifies it.

use std::fs;
use std::path::Path;

/// How many key frames to pull out of one clip, evenly spaced across the
/// whole recording (including the first and last frame). Deliberately
/// small: this is a cheap, representative sample for a future automated
/// consumer, not a full replay -- if per-frame analysis of an entire clip
/// is ever needed, that's a different, heavier feature than this one.
const MAX_KEYFRAMES: usize = 6;

#[derive(Debug, PartialEq, Eq)]
pub enum VideoError {
    Empty,
    Truncated,
}

/// One decoded frame's timestamp (ms since the event started) and JPEG
/// bytes, borrowed from the raw `.bin` buffer -- no copying until a frame
/// is actually selected as a keyframe and written out.
#[derive(Debug)]
struct Frame<'a> {
    timestamp_ms: u32,
    jpeg: &'a [u8],
}

/// Parses every frame out of `raw` -- the same `[frame_len][timestamp_ms]
/// [jpeg]` sequence `decode_raw_capture.py` parses for the USB export,
/// except the server's stored `.bin` file *is* that sequence directly (no
/// enclosing `RAW EXPORT BEGIN <n>` marker -- that framing is specific to
/// the serial-dump path, not the network upload).
fn parse_frames(raw: &[u8]) -> Result<Vec<Frame<'_>>, VideoError> {
    let mut frames = Vec::new();
    let mut offset = 0;
    while offset + 8 <= raw.len() {
        let frame_len = u32::from_le_bytes(raw[offset..offset + 4].try_into().unwrap()) as usize;
        let timestamp_ms = u32::from_le_bytes(raw[offset + 4..offset + 8].try_into().unwrap());
        offset += 8;
        if offset + frame_len > raw.len() {
            return Err(VideoError::Truncated);
        }
        frames.push(Frame {
            timestamp_ms,
            jpeg: &raw[offset..offset + frame_len],
        });
        offset += frame_len;
    }
    if frames.is_empty() {
        return Err(VideoError::Empty);
    }
    Ok(frames)
}

/// Picks up to `MAX_KEYFRAMES` frames evenly spaced across `frames`,
/// always including the first and last frame when there are more frames
/// than that -- if there are already `MAX_KEYFRAMES` or fewer, every frame
/// is kept (nothing to thin out).
fn select_keyframes<'a, 'b>(frames: &'b [Frame<'a>]) -> Vec<&'b Frame<'a>> {
    if frames.len() <= MAX_KEYFRAMES {
        return frames.iter().collect();
    }
    let mut indices: Vec<usize> = (0..MAX_KEYFRAMES)
        .map(|i| i * (frames.len() - 1) / (MAX_KEYFRAMES - 1))
        .collect();
    indices.dedup();
    indices.into_iter().map(|i| &frames[i]).collect()
}

/// Reads `video_path` (a stored event's raw `_video.bin` part), extracts
/// up to `MAX_KEYFRAMES` representative stills, and writes each as
/// `event_<timestamp>_keyframe_<n>.jpg` beside it -- sharing the same
/// `event_<timestamp>` prefix `retention::event_key` already groups by, so
/// keyframes are naturally cleaned up together with the rest of the event
/// once it expires, with no changes needed there. Never returns an error
/// or panics -- every failure (unreadable file, malformed frames, a path
/// that doesn't match the expected naming) is logged and just means no
/// keyframes get written; see the module doc for why.
pub fn extract_keyframes(video_path: &Path) {
    let Some(stem) = video_path.to_str().and_then(|s| s.strip_suffix("_video.bin")) else {
        println!(
            "video: {} doesn't match the expected *_video.bin naming, skipping",
            video_path.display()
        );
        return;
    };

    let raw = match fs::read(video_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            println!("video: failed to read {}: {e}", video_path.display());
            return;
        }
    };

    let frames = match parse_frames(&raw) {
        Ok(frames) => frames,
        Err(e) => {
            println!("video: failed to parse frames from {}: {e:?}", video_path.display());
            return;
        }
    };

    let keyframes = select_keyframes(&frames);
    let mut written = 0;
    for (i, frame) in keyframes.iter().enumerate() {
        let path = format!("{stem}_keyframe_{i}.jpg");
        match fs::write(&path, frame.jpeg) {
            Ok(()) => written += 1,
            Err(e) => println!("video: failed to write {path}: {e}"),
        }
    }
    println!(
        "video: extracted {written}/{} keyframe(s) from {} ({} total frame(s), timestamps {:?}ms)",
        keyframes.len(),
        video_path.display(),
        frames.len(),
        keyframes.iter().map(|f| f.timestamp_ms).collect::<Vec<_>>()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_dir(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let dir = std::env::temp_dir().join(format!("camera_server_test_video_{label}_{unique}"));
        fs::create_dir_all(&dir).expect("scratch dir should be creatable");
        dir
    }

    fn encode_frames(frames: &[(u32, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (timestamp_ms, jpeg) in frames {
            buf.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
            buf.extend_from_slice(&timestamp_ms.to_le_bytes());
            buf.extend_from_slice(jpeg);
        }
        buf
    }

    #[test]
    fn parses_a_few_frames_correctly() {
        let raw = encode_frames(&[(0, b"frame-zero"), (33, b"frame-one"), (66, b"frame-two")]);
        let frames = parse_frames(&raw).expect("well-formed frames should parse");
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].timestamp_ms, 0);
        assert_eq!(frames[0].jpeg, b"frame-zero");
        assert_eq!(frames[2].timestamp_ms, 66);
        assert_eq!(frames[2].jpeg, b"frame-two");
    }

    #[test]
    fn rejects_an_empty_buffer() {
        assert_eq!(parse_frames(&[]).unwrap_err(), VideoError::Empty);
    }

    #[test]
    fn rejects_a_truncated_final_frame() {
        let mut raw = encode_frames(&[(0, b"frame-zero")]);
        raw.extend_from_slice(&100u32.to_le_bytes()); // claims a 100-byte frame
        raw.extend_from_slice(&123u32.to_le_bytes()); // timestamp
        raw.extend_from_slice(b"way too short"); // far fewer bytes actually present
        assert_eq!(parse_frames(&raw).unwrap_err(), VideoError::Truncated);
    }

    #[test]
    fn keeps_every_frame_when_at_or_under_the_max() {
        let raw = encode_frames(&[(0, b"a"), (10, b"b"), (20, b"c")]);
        let frames = parse_frames(&raw).unwrap();
        let selected = select_keyframes(&frames);
        assert_eq!(selected.len(), 3);
    }

    #[test]
    fn thins_a_long_clip_to_the_max_keeping_first_and_last() {
        let owned: Vec<(u32, Vec<u8>)> = (0..50).map(|i| (i * 33, vec![b'a' + (i % 26) as u8])).collect();
        let borrowed: Vec<(u32, &[u8])> = owned.iter().map(|(ts, jpeg)| (*ts, jpeg.as_slice())).collect();
        let raw = encode_frames(&borrowed);
        let frames = parse_frames(&raw).unwrap();
        let selected = select_keyframes(&frames);

        assert!(selected.len() <= MAX_KEYFRAMES);
        assert_eq!(selected.first().unwrap().timestamp_ms, 0, "must include the first frame");
        assert_eq!(
            selected.last().unwrap().timestamp_ms,
            49 * 33,
            "must include the last frame"
        );
    }

    #[test]
    fn extract_keyframes_writes_expected_files_beside_the_source() {
        let dir = scratch_dir("success");
        let video_path = dir.join("event_123_video.bin");
        let raw = encode_frames(&[(0, b"frame-zero"), (33, b"frame-one"), (66, b"frame-two")]);
        fs::write(&video_path, &raw).unwrap();

        extract_keyframes(&video_path);

        assert!(video_path.exists(), "original .bin must never be touched");
        assert_eq!(fs::read(&video_path).unwrap(), raw);

        for i in 0..3 {
            let keyframe = dir.join(format!("event_123_keyframe_{i}.jpg"));
            assert!(keyframe.exists(), "expected {keyframe:?} to exist");
        }
        assert_eq!(fs::read(dir.join("event_123_keyframe_1.jpg")).unwrap(), b"frame-one");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writes_nothing_when_the_source_is_malformed() {
        let dir = scratch_dir("malformed");
        let video_path = dir.join("event_456_video.bin");
        fs::write(&video_path, b"not a valid frame stream at all, too short").unwrap();

        extract_keyframes(&video_path);

        let entries: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(entries.len(), 1, "only the untouched original file should remain");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writes_nothing_when_the_source_file_is_missing() {
        let dir = scratch_dir("missing");
        let video_path = dir.join("event_789_video.bin");
        // Deliberately never written.

        extract_keyframes(&video_path);

        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn skips_a_path_that_does_not_match_the_expected_naming() {
        let dir = scratch_dir("bad_naming");
        let video_path = dir.join("not_the_expected_shape.bin");
        fs::write(&video_path, encode_frames(&[(0, b"x")])).unwrap();

        extract_keyframes(&video_path);

        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "only the untouched original file should remain");

        fs::remove_dir_all(&dir).ok();
    }
}
