//! Server-side key-frame extraction from a stored event's raw video part.
//!
//! Recorded clips are currently stored exactly as `firmware::recorder::
//! PsramRecorder` wrote them: `[frame_len: u32 LE][timestamp_ms: u32 LE]
//! [frame_len bytes of JPEG]` repeated (the `shared` crate's
//! `Encoding::RecorderFrames`), the same wire format
//! `scripts/decode_raw_capture.py` already parses for the USB export path
//! -- viewing one still requires this project's existing decode/assemble
//! tooling. This module doesn't do a full video conversion (no `ffmpeg`
//! dependency for the core upload path) -- it pulls out a handful of
//! representative JPEG stills for AI, and can also assemble the full raw
//! clip into a phone-playable `.mp4` via `ffmpeg` when available.
//!
//! Runs after the event is already stored, on its own detached background
//! thread (see `main.rs`) -- exactly like `ai::analyze_and_save`: this
//! server's accept loop is single-threaded, so this must never run
//! in-line with request handling, and conversion success or failure must
//! never affect whether the upload itself succeeded. The original `.bin`
//! file is always kept regardless -- nothing here ever deletes or
//! modifies it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// How many key frames to pull out of one clip, evenly spaced across the
/// whole recording (including the first and last frame). Deliberately
/// small: this is a cheap, representative sample for a future automated
/// consumer, not a full replay -- if per-frame analysis of an entire clip
/// is ever needed, that's a different, heavier feature than this one.
const MAX_KEYFRAMES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayColor {
    White,
    Yellow,
    Red,
    Green,
}

impl OverlayColor {
    fn ffmpeg(self) -> &'static str {
        match self {
            OverlayColor::White => "white",
            OverlayColor::Yellow => "yellow",
            OverlayColor::Red => "red",
            OverlayColor::Green => "lime",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayLabel {
    pub text: String,
    pub color: OverlayColor,
}

impl OverlayLabel {
    pub fn new(text: impl Into<String>, color: OverlayColor) -> Self {
        Self {
            text: text.into(),
            color,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayRegion {
    pub label: String,
    pub color: OverlayColor,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl OverlayRegion {
    pub fn new(label: impl Into<String>, color: OverlayColor, x: u16, y: u16, w: u16, h: u16) -> Self {
        Self {
            label: label.into(),
            color,
            x,
            y,
            w,
            h,
        }
    }
}

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
/// keyframes get written; see the module doc for why. Returns the paths
/// that were actually written (possibly empty on any failure), so a
/// caller -- `ai::analyze_and_save`, via `main.rs` -- knows exactly what's
/// available to analyze alongside the thumbnail; an empty result is a
/// normal, expected input there, not an error to propagate.
pub fn extract_keyframes(video_path: &Path) -> Vec<PathBuf> {
    let Some(stem) = video_path.to_str().and_then(|s| s.strip_suffix("_video.bin")) else {
        println!(
            "video: {} doesn't match the expected *_video.bin naming, skipping",
            video_path.display()
        );
        return Vec::new();
    };

    let raw = match fs::read(video_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            println!("video: failed to read {}: {e}", video_path.display());
            return Vec::new();
        }
    };

    let frames = match parse_frames(&raw) {
        Ok(frames) => frames,
        Err(e) => {
            println!("video: failed to parse frames from {}: {e:?}", video_path.display());
            return Vec::new();
        }
    };

    let keyframes = select_keyframes(&frames);
    let mut written_paths = Vec::new();
    for (i, frame) in keyframes.iter().enumerate() {
        let path = format!("{stem}_keyframe_{i}.jpg");
        match fs::write(&path, frame.jpeg) {
            Ok(()) => written_paths.push(PathBuf::from(path)),
            Err(e) => println!("video: failed to write {path}: {e}"),
        }
    }
    println!(
        "video: extracted {}/{} keyframe(s) from {} ({} total frame(s), timestamps {:?}ms)",
        written_paths.len(),
        keyframes.len(),
        video_path.display(),
        frames.len(),
        keyframes.iter().map(|f| f.timestamp_ms).collect::<Vec<_>>()
    );
    written_paths
}

/// Converts a stored raw `_video.bin` recorder-frame stream into a normal
/// phone-playable H.264 MP4 beside it (`event_<id>_video.mp4`) using
/// `ffmpeg`. The original `.bin` is never modified or deleted. Any failure
/// is logged and returned as `None`: conversion is a convenience for human
/// viewing/Telegram, never part of whether event upload/storage succeeded.
pub fn convert_to_mp4(video_path: &Path) -> Option<PathBuf> {
    let output_path = video_path.with_extension("mp4");
    if output_path.is_file() {
        return Some(output_path);
    }

    let raw = match fs::read(video_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            println!("video: failed to read {} for mp4 conversion: {e}", video_path.display());
            return None;
        }
    };
    let frames = match parse_frames(&raw) {
        Ok(frames) => frames,
        Err(e) => {
            println!("video: failed to parse {} for mp4 conversion: {e:?}", video_path.display());
            return None;
        }
    };

    let fps = average_fps(&frames).unwrap_or(10.0).clamp(1.0, 30.0);
    let Some(parent) = video_path.parent() else {
        println!("video: {} has no parent directory for mp4 conversion", video_path.display());
        return None;
    };
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let tmp_dir = parent.join(format!(".tmp_video_mp4_{unique}"));
    if let Err(e) = fs::create_dir_all(&tmp_dir) {
        println!("video: failed to create temp dir {}: {e}", tmp_dir.display());
        return None;
    }

    let result = convert_frames_to_mp4(&frames, &tmp_dir, &output_path, fps);
    if let Err(e) = fs::remove_dir_all(&tmp_dir) {
        println!("video: failed to remove temp dir {}: {e}", tmp_dir.display());
    }

    match result {
        Ok(()) => {
            println!(
                "video: converted {} frame(s) from {} to {} at {:.2} fps",
                frames.len(),
                video_path.display(),
                output_path.display(),
                fps
            );
            Some(output_path)
        }
        Err(e) => {
            println!("video: mp4 conversion failed for {}: {e}", video_path.display());
            let _ = fs::remove_file(&output_path);
            None
        }
    }
}

/// Runs the optional YOLO/ByteTrack sidecar to create a real frame-level,
/// temporally stabilized locked video plus its tracks JSON sidecar.
/// If Python, Ultralytics, OpenCV, the model, or tracking fails, this logs
/// the reason and returns `None`; callers can then fall back to the plain
/// MP4 or the older static-HUD overlay. The source MP4 is never modified.
pub fn lock_mp4_with_tracker(
    input_path: &Path,
    threat_level: TrackerThreatLevel,
    concerning_behavior: bool,
) -> Option<PathBuf> {
    let output_path = locked_output_path(input_path, Some(threat_level))?;
    if output_path.is_file() {
        return Some(output_path);
    }
    let tracks_path = tracks_output_path(input_path, Some(threat_level))?;
    let script_path = tracker_script_path();
    if !script_path.is_file() {
        println!("video: tracker sidecar not found at {}", script_path.display());
        return None;
    }

    let python = tracker_python();
    let model = std::env::var("FRIDAY_YOLO_MODEL").unwrap_or_else(|_| "yolo11m.pt".to_string());
    let tracker = std::env::var("FRIDAY_YOLO_TRACKER").unwrap_or_else(|_| "bytetrack.yaml".to_string());
    let conf = std::env::var("FRIDAY_YOLO_CONF").unwrap_or_else(|_| "0.08".to_string());
    let imgsz = std::env::var("FRIDAY_YOLO_IMGSZ").unwrap_or_else(|_| "960".to_string());
    let body_smoothing =
        std::env::var("FRIDAY_BODY_SMOOTHING").unwrap_or_else(|_| "0.30".to_string());
    let face_smoothing =
        std::env::var("FRIDAY_FACE_SMOOTHING").unwrap_or_else(|_| "0.22".to_string());
    let track_hold_frames =
        std::env::var("FRIDAY_TRACK_HOLD_FRAMES").unwrap_or_else(|_| "5".to_string());
    let hand_conf = std::env::var("FRIDAY_HAND_CONF").unwrap_or_else(|_| "0.35".to_string());
    let gesture_confirm_frames =
        std::env::var("FRIDAY_GESTURE_CONFIRM_FRAMES").unwrap_or_else(|_| "2".to_string());
    let gesture_hold_frames =
        std::env::var("FRIDAY_GESTURE_HOLD_FRAMES").unwrap_or_else(|_| "18".to_string());
    let threat_preroll_frames =
        std::env::var("FRIDAY_THREAT_PREROLL_FRAMES").unwrap_or_else(|_| "18".to_string());

    let mut command = Command::new(&python);
    command
        .arg(&script_path)
        .arg("--input")
        .arg(input_path)
        .arg("--output")
        .arg(&output_path)
        .arg("--tracks")
        .arg(&tracks_path)
        .arg("--model")
        .arg(&model)
        .arg("--tracker")
        .arg(&tracker)
        .arg("--conf")
        .arg(&conf)
        .arg("--imgsz")
        .arg(&imgsz)
        .arg("--body-smoothing")
        .arg(&body_smoothing)
        .arg("--face-smoothing")
        .arg(&face_smoothing)
        .arg("--track-hold-frames")
        .arg(&track_hold_frames)
        .arg("--hand-conf")
        .arg(&hand_conf)
        .arg("--gesture-confirm-frames")
        .arg(&gesture_confirm_frames)
        .arg("--gesture-hold-frames")
        .arg(&gesture_hold_frames)
        .arg("--threat-preroll-frames")
        .arg(&threat_preroll_frames)
        .arg("--threat-level")
        .arg(threat_level.as_str());
    // Passed independently of `threat_level`: `tracker_threat_level` (in
    // `notify.rs`) collapses concerning_object/concerning_behavior/importance
    // into one 3-way level, which loses whether concerning_behavior is *also*
    // true for a "threat" (concerning_object) event -- and that's exactly what
    // decides whether person/face downgrades to yellow or white once the
    // threat-hold window expires (see the tracker script's `ThreatLatch`).
    if concerning_behavior {
        command.arg("--concerning-behavior");
    }
    let output = command.output().map_err(|e| format!("start tracker sidecar: {e}"));

    match output {
        Ok(output) if output.status.success() && output_path.is_file() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.trim().is_empty() {
                println!("video: tracker sidecar: {}", stdout.trim());
            }
            Some(output_path)
        }
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!(
                "video: tracker sidecar failed for {} with {}: {}{}{}",
                input_path.display(),
                output.status,
                stdout.trim(),
                if stdout.trim().is_empty() || stderr.trim().is_empty() {
                    ""
                } else {
                    " -- "
                },
                stderr.trim()
            );
            let _ = fs::remove_file(&output_path);
            let _ = fs::remove_file(&tracks_path);
            None
        }
        Err(e) => {
            println!("video: tracker sidecar failed for {}: {e}", input_path.display());
            None
        }
    }
}

/// Creates `event_<id>_alert.jpg` from the clearest human keyframe using
/// the lightweight YuNet face selector sidecar. This is used for Telegram
/// alert photos so the first notification shows a recognizable human frame
/// instead of the PIR trigger frame where the person may only be half in view.
/// Returns `None` on any sidecar/dependency/no-face failure so callers can
/// fall back to the normal keyframe/thumbnail choice.
pub fn select_alert_thumbnail(thumbnail_path: &Path, keyframe_paths: &[PathBuf]) -> Option<PathBuf> {
    let output_path = alert_output_path(thumbnail_path)?;
    if output_path.is_file() {
        return Some(output_path);
    }
    let script_path = alert_selector_script_path();
    if !script_path.is_file() {
        println!("video: alert-frame sidecar not found at {}", script_path.display());
        return None;
    }

    let mut images: Vec<&Path> = keyframe_paths.iter().map(PathBuf::as_path).collect();
    images.push(thumbnail_path);
    let python = tracker_python();
    let output = Command::new(&python)
        .arg(&script_path)
        .arg("--output")
        .arg(&output_path)
        .args(&images)
        .output()
        .map_err(|e| format!("start alert-frame sidecar: {e}"));

    match output {
        Ok(output) if output.status.success() && output_path.is_file() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.trim().is_empty() {
                println!("video: alert-frame sidecar: {}", stdout.trim());
            }
            Some(output_path)
        }
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!(
                "video: alert-frame sidecar failed for {} with {}: {}{}{}",
                thumbnail_path.display(),
                output.status,
                stdout.trim(),
                if stdout.trim().is_empty() || stderr.trim().is_empty() {
                    ""
                } else {
                    " -- "
                },
                stderr.trim()
            );
            let _ = fs::remove_file(&output_path);
            None
        }
        Err(e) => {
            println!("video: alert-frame sidecar failed for {}: {e}", thumbnail_path.display());
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerThreatLevel {
    Normal,
    Minimal,
    Threat,
}

impl TrackerThreatLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            TrackerThreatLevel::Normal => "normal",
            TrackerThreatLevel::Minimal => "minimal",
            TrackerThreatLevel::Threat => "threat",
        }
    }
}

/// Burns a simple FRIDAY/Person-of-Interest-style HUD into an already
/// playable MP4. This is intentionally a server-side viewing feature: the
/// original raw `.bin` and plain `.mp4` are kept unchanged, and failures
/// fall back to the normal video path. The labels come from the event AI
/// analysis; this does not yet perform true per-frame object tracking or
/// exact bounding-box detection.
pub fn annotate_mp4(input_path: &Path, labels: &[OverlayLabel], regions: &[OverlayRegion]) -> Option<PathBuf> {
    if labels.is_empty() && regions.is_empty() {
        return Some(input_path.to_path_buf());
    }

    let output_path = annotated_output_path(input_path)?;
    if output_path.is_file() {
        return Some(output_path);
    }

    let filter = overlay_filter(labels, regions);
    let Some(file_name) = output_path.file_name().and_then(|s| s.to_str()) else {
        println!("video: invalid annotated output file name: {}", output_path.display());
        return None;
    };
    let tmp_output = output_path.with_file_name(format!(".{file_name}.tmp.mp4"));

    let output = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(input_path)
        .arg("-vf")
        .arg(&filter)
        .arg("-c:v")
        .arg("libx264")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-movflags")
        .arg("+faststart")
        .arg("-an")
        .arg(&tmp_output)
        .output()
        .map_err(|e| format!("start ffmpeg: {e}"));

    match output {
        Ok(output) if output.status.success() => match fs::rename(&tmp_output, &output_path) {
            Ok(()) => {
                println!(
                    "video: created annotated video {} from {}",
                    output_path.display(),
                    input_path.display()
                );
                Some(output_path)
            }
            Err(e) => {
                println!(
                    "video: failed to rename annotated temp output {} -> {}: {e}",
                    tmp_output.display(),
                    output_path.display()
                );
                let _ = fs::remove_file(&tmp_output);
                None
            }
        },
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!("video: annotated mp4 failed for {}: {}", input_path.display(), stderr);
            let _ = fs::remove_file(&tmp_output);
            None
        }
        Err(e) => {
            println!("video: annotated mp4 failed for {}: {e}", input_path.display());
            let _ = fs::remove_file(&tmp_output);
            None
        }
    }
}

fn annotated_output_path(input_path: &Path) -> Option<PathBuf> {
    locked_output_path(input_path, None)
}

fn locked_output_path(input_path: &Path, threat_level: Option<TrackerThreatLevel>) -> Option<PathBuf> {
    let name = input_path.file_name()?.to_str()?;
    let suffix = threat_level
        .map(|level| format!("_locked_reactive_{}", level.as_str()))
        .unwrap_or_else(|| "_locked_frame".to_string());
    let annotated = if let Some(stem) = name.strip_suffix("_video.mp4") {
        format!("{stem}{suffix}.mp4")
    } else if let Some(stem) = name.strip_suffix(".mp4") {
        format!("{stem}{suffix}.mp4")
    } else {
        return None;
    };
    Some(input_path.with_file_name(annotated))
}

fn tracks_output_path(input_path: &Path, threat_level: Option<TrackerThreatLevel>) -> Option<PathBuf> {
    let name = input_path.file_name()?.to_str()?;
    let suffix = threat_level
        .map(|level| format!("_tracks_reactive_{}", level.as_str()))
        .unwrap_or_else(|| "_tracks_frame".to_string());
    let tracks = if let Some(stem) = name.strip_suffix("_video.mp4") {
        format!("{stem}{suffix}.json")
    } else if let Some(stem) = name.strip_suffix(".mp4") {
        format!("{stem}{suffix}.json")
    } else {
        return None;
    };
    Some(input_path.with_file_name(tracks))
}

fn alert_output_path(thumbnail_path: &Path) -> Option<PathBuf> {
    let name = thumbnail_path.file_name()?.to_str()?;
    let stem = name.strip_suffix("_thumbnail.jpg")?;
    Some(thumbnail_path.with_file_name(format!("{stem}_alert.jpg")))
}

fn tracker_script_path() -> PathBuf {
    if let Ok(path) = std::env::var("FRIDAY_TRACKER_SCRIPT") {
        return PathBuf::from(path);
    }
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .unwrap_or(manifest_dir)
        .join("scripts")
        .join("track_video.py")
}

fn alert_selector_script_path() -> PathBuf {
    if let Ok(path) = std::env::var("FRIDAY_ALERT_FRAME_SCRIPT") {
        return PathBuf::from(path);
    }
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .unwrap_or(manifest_dir)
        .join("scripts")
        .join("select_alert_frame.py")
}

fn tracker_python() -> String {
    if let Ok(path) = std::env::var("FRIDAY_TRACKER_PYTHON") {
        return path;
    }
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.parent().unwrap_or(manifest_dir);
    let venv_python = repo_root.join(".venv-tracker").join("bin").join("python");
    if venv_python.is_file() {
        venv_python.to_string_lossy().into_owned()
    } else {
        "python3".to_string()
    }
}

fn overlay_filter(labels: &[OverlayLabel], regions: &[OverlayRegion]) -> String {
    let primary = labels
        .iter()
        .find(|label| label.color == OverlayColor::Red)
        .or_else(|| labels.iter().find(|label| label.color == OverlayColor::Yellow))
        .or_else(|| labels.first())
        .map(|label| label.color)
        .unwrap_or(OverlayColor::White);
    let primary_color = primary.ffmpeg();

    let mut filters = vec![
        format!("drawbox=x=12:y=12:w=iw-24:h=ih-24:color={primary_color}@0.85:t=4"),
        "drawtext=text='FRIDAY VISION':x=24:y=24:fontsize=22:fontcolor=cyan:box=1:boxcolor=black@0.60:boxborderw=8".to_string(),
    ];

    if regions.is_empty() {
        filters.push(format!("drawbox=x=305:y=20:w=300:h=365:color={primary_color}@0.75:t=4"));
        filters.push(format!("drawbox=x=365:y=48:w=220:h=175:color={primary_color}@0.95:t=6"));
        filters.push(format!("drawtext=text='PERSON REGION':x=305:y=226:fontsize=18:fontcolor={primary_color}:box=1:boxcolor=black@0.65:boxborderw=6"));
        filters.push(format!("drawtext=text='FACE / HEAD':x=365:y=20:fontsize=18:fontcolor={primary_color}:box=1:boxcolor=black@0.65:boxborderw=6"));
    } else {
        for region in regions.iter().take(8) {
            let (x, y, w, h) = normalized_region_to_640x480(region);
            let color = region.color.ffmpeg();
            let label_y = y.saturating_sub(28);
            filters.push(format!("drawbox=x={x}:y={y}:w={w}:h={h}:color={color}@0.95:t=5"));
            filters.push(format!(
                "drawtext=text='{}':x={x}:y={label_y}:fontsize=18:fontcolor={color}:box=1:boxcolor=black@0.70:boxborderw=6",
                escape_drawtext(&region.label.to_ascii_uppercase())
            ));
        }
    }

    for (i, label) in labels.iter().take(6).enumerate() {
        let y = 64 + i * 32;
        filters.push(format!(
            "drawtext=text='{}':x=24:y={y}:fontsize=20:fontcolor={}:box=1:boxcolor=black@0.55:boxborderw=7",
            escape_drawtext(&label.text),
            label.color.ffmpeg()
        ));
    }

    filters.join(",")
}

fn normalized_region_to_640x480(region: &OverlayRegion) -> (u16, u16, u16, u16) {
    let x = region.x.min(999) as u32 * 640 / 1000;
    let y = region.y.min(999) as u32 * 480 / 1000;
    let max_w = 640u32.saturating_sub(x).max(1);
    let max_h = 480u32.saturating_sub(y).max(1);
    let min_w = 12.min(max_w);
    let min_h = 12.min(max_h);
    let w = (region.w.clamp(20, 1000) as u32 * 640 / 1000).clamp(min_w, max_w);
    let h = (region.h.clamp(20, 1000) as u32 * 480 / 1000).clamp(min_h, max_h);
    (x as u16, y as u16, w as u16, h as u16)
}

fn escape_drawtext(text: &str) -> String {
    text.chars()
        .flat_map(|ch| match ch {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '\'' => "\\'".chars().collect::<Vec<_>>(),
            ':' => "\\:".chars().collect::<Vec<_>>(),
            ',' => "\\,".chars().collect::<Vec<_>>(),
            '[' => "\\[".chars().collect::<Vec<_>>(),
            ']' => "\\]".chars().collect::<Vec<_>>(),
            _ => vec![ch],
        })
        .collect()
}

fn average_fps(frames: &[Frame<'_>]) -> Option<f64> {
    if frames.len() < 2 {
        return None;
    }
    let first = frames.first()?.timestamp_ms;
    let last = frames.last()?.timestamp_ms;
    let elapsed_ms = last.checked_sub(first)?;
    if elapsed_ms == 0 {
        return None;
    }
    Some((frames.len() - 1) as f64 / (elapsed_ms as f64 / 1000.0))
}

fn convert_frames_to_mp4(frames: &[Frame<'_>], tmp_dir: &Path, output_path: &Path, fps: f64) -> Result<(), String> {
    for (i, frame) in frames.iter().enumerate() {
        let path = tmp_dir.join(format!("frame_{i:05}.jpg"));
        fs::write(&path, frame.jpeg).map_err(|e| format!("write {}: {e}", path.display()))?;
    }

    let Some(file_name) = output_path.file_name().and_then(|s| s.to_str()) else {
        return Err(format!("invalid output file name: {}", output_path.display()));
    };
    let tmp_output = output_path.with_file_name(format!(".{file_name}.tmp.mp4"));
    let input_pattern = tmp_dir.join("frame_%05d.jpg");
    let output = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-framerate")
        .arg(format!("{fps:.3}"))
        .arg("-i")
        .arg(&input_pattern)
        .arg("-c:v")
        .arg("libx264")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-movflags")
        .arg("+faststart")
        .arg(&tmp_output)
        .output()
        .map_err(|e| format!("start ffmpeg: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&tmp_output);
        return Err(format!("ffmpeg exited with {}: {stderr}", output.status));
    }

    fs::rename(&tmp_output, output_path)
        .map_err(|e| format!("rename {} -> {}: {e}", tmp_output.display(), output_path.display()))
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

        let written = extract_keyframes(&video_path);

        assert!(video_path.exists(), "original .bin must never be touched");
        assert_eq!(fs::read(&video_path).unwrap(), raw);

        assert_eq!(written.len(), 3);
        for (i, written_path) in written.iter().enumerate() {
            let keyframe = dir.join(format!("event_123_keyframe_{i}.jpg"));
            assert!(keyframe.exists(), "expected {keyframe:?} to exist");
            assert_eq!(written_path, &keyframe, "returned paths should match what was written, in order");
        }
        assert_eq!(fs::read(dir.join("event_123_keyframe_1.jpg")).unwrap(), b"frame-one");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writes_nothing_when_the_source_is_malformed() {
        let dir = scratch_dir("malformed");
        let video_path = dir.join("event_456_video.bin");
        fs::write(&video_path, b"not a valid frame stream at all, too short").unwrap();

        let written = extract_keyframes(&video_path);
        assert!(written.is_empty());

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

    #[test]
    fn reactive_tracker_outputs_use_a_fresh_cache_name() {
        let input = Path::new("/tmp/event_123_video.mp4");

        assert_eq!(
            locked_output_path(input, Some(TrackerThreatLevel::Normal)),
            Some(PathBuf::from("/tmp/event_123_locked_reactive_normal.mp4"))
        );
        assert_eq!(
            tracks_output_path(input, Some(TrackerThreatLevel::Threat)),
            Some(PathBuf::from("/tmp/event_123_tracks_reactive_threat.json"))
        );
    }
}
