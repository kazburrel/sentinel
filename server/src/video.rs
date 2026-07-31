//! Server-side key-frame extraction from a stored event's raw video part.
//!
//! Recorded clips are currently stored exactly as `firmware::recorder::
//! PsramRecorder` wrote them: `[frame_len: u32 LE][timestamp_ms: u32 LE]
//! [frame_len bytes of JPEG]` repeated (the `shared` crate's
//! `Encoding::RecorderFrames`), the same wire format
//! the Rust `server decode-raw` command also parses for the USB export path
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
/// [jpeg]` sequence the Rust `server decode-raw` command parses for USB export,
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

/// Runs Rust-native YOLO/YuNet inference and temporal stabilization to
/// create a real frame-level locked video plus its tracks JSON. The source
/// MP4 and recorder stream are never modified.
pub fn lock_mp4_with_tracker(
    input_path: &Path,
    threat_level: TrackerThreatLevel,
    concerning_behavior: bool,
    trusted_admin: bool,
) -> Option<PathBuf> {
    let output_path = locked_output_path(input_path, Some(threat_level))?;
    if output_path.is_file() {
        return Some(output_path);
    }
    let tracks_path = tracks_output_path(input_path, Some(threat_level))?;
    match lock_mp4_in_rust(
        input_path,
        &output_path,
        &tracks_path,
        threat_level,
        concerning_behavior,
        trusted_admin,
    ) {
        Ok(()) => Some(output_path),
        Err(error) => {
            println!("video: Rust tracker failed for {}: {error}", input_path.display());
            let _ = fs::remove_file(&output_path);
            let _ = fs::remove_file(&tracks_path);
            None
        }
    }
}

/// Creates `event_<id>_alert.jpg` from the clearest human keyframe using
/// Rust-native YuNet face detection. This is used for Telegram
/// alert photos so the first notification shows a recognizable human frame
/// instead of the PIR trigger frame where the person may only be half in view.
/// Returns `None` on any model/no-face failure so callers can
/// fall back to the normal keyframe/thumbnail choice.
pub fn select_alert_thumbnail(thumbnail_path: &Path, keyframe_paths: &[PathBuf]) -> Option<PathBuf> {
    let output_path = alert_output_path(thumbnail_path)?;
    if output_path.is_file() {
        return Some(output_path);
    }
    let mut images: Vec<&Path> = keyframe_paths.iter().map(PathBuf::as_path).collect();
    images.push(thumbnail_path);
    let mut best: Option<(f32, &Path)> = None;
    for path in images {
        let Ok(image) = image::open(path) else {
            continue;
        };
        match crate::vision::best_face_across_rotations(&image, 0.45) {
            Ok(Some(face)) if best.is_none_or(|candidate| face.quality > candidate.0) => {
                best = Some((face.quality, path));
            }
            Ok(_) => {}
            Err(error) => {
                println!("video: Rust alert-frame detection failed: {error}");
                return None;
            }
        }
    }
    let (_, selected) = best?;
    let temporary = output_path.with_file_name(format!(
        ".{}.tmp",
        output_path.file_name().and_then(|name| name.to_str()).unwrap_or("alert.jpg")
    ));
    fs::copy(selected, &temporary).ok()?;
    fs::rename(&temporary, &output_path).ok()?;
    println!("video: Rust alert-frame selector chose {}", selected.display());
    Some(output_path)
}

#[derive(Debug, Clone, serde::Serialize)]
struct TrackedDetection {
    track_id: Option<u32>,
    kind: String,
    label: String,
    confidence: f32,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    source: String,
    stabilized: bool,
    held: bool,
}

impl TrackedDetection {
    fn bounds(&self) -> crate::vision::Rect {
        crate::vision::Rect {
            x1: self.x1 as f32,
            y1: self.y1 as f32,
            x2: self.x2 as f32,
            y2: self.y2 as f32,
        }
    }

    fn set_bounds(&mut self, bounds: crate::vision::Rect, width: u32, height: u32) {
        let bounds = bounds.clamp(width, height);
        self.x1 = bounds.x1.round() as i32;
        self.y1 = bounds.y1.round() as i32;
        self.x2 = bounds.x2.round() as i32;
        self.y2 = bounds.y2.round() as i32;
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct TrackedFrame {
    frame_index: usize,
    timestamp_ms: u32,
    event_threat_level: String,
    rendered_threat_level: String,
    threat_preroll: bool,
    detections: Vec<TrackedDetection>,
}

#[derive(Debug, Clone)]
struct BodyState {
    template: TrackedDetection,
    bounds: crate::vision::Rect,
    velocity_x: f32,
    velocity_y: f32,
    missed: usize,
}

struct BodyTracker {
    states: std::collections::HashMap<u32, BodyState>,
    next_id: u32,
    smoothing: f32,
    hold_frames: usize,
}

impl BodyTracker {
    fn new(smoothing: f32, hold_frames: usize) -> Self {
        Self {
            states: std::collections::HashMap::new(),
            next_id: 1,
            smoothing: smoothing.clamp(0.05, 1.0),
            hold_frames,
        }
    }

    fn update(
        &mut self,
        detections: Vec<crate::vision::ObjectDetection>,
        width: u32,
        height: u32,
    ) -> Vec<TrackedDetection> {
        let mut people: Vec<_> = detections
            .iter()
            .filter(|detection| detection.kind == crate::vision::ObjectKind::Person)
            .cloned()
            .collect();
        people.sort_by(|left, right| right.bounds.area().total_cmp(&left.bounds.area()));
        let mut suppressed = Vec::new();
        for person in people {
            if suppressed.iter().any(|kept: &crate::vision::ObjectDetection| {
                intersection_over_smaller(person.bounds, kept.bounds) > 0.45
            }) {
                continue;
            }
            suppressed.push(person);
        }

        let person_bounds: Vec<crate::vision::Rect> = suppressed.iter().map(|person| person.bounds).collect();
        let mut output: Vec<TrackedDetection> = detections
            .into_iter()
            .filter(|detection| detection.kind != crate::vision::ObjectKind::Person)
            .filter(|detection| {
                !matches!(
                    detection.kind,
                    crate::vision::ObjectKind::Animal | crate::vision::ObjectKind::Vehicle
                ) || person_bounds
                    .iter()
                    .all(|person| intersection_over_smaller(detection.bounds, *person) < 0.50)
            })
            .map(object_detection_for_render)
            .collect();
        let mut seen = std::collections::HashSet::new();
        for person in suppressed {
            let best = self
                .states
                .iter()
                .filter(|(track_id, _)| !seen.contains(*track_id))
                .map(|(track_id, state)| (*track_id, crate::vision::intersection_over_union(person.bounds, state.bounds)))
                .filter(|(_, overlap)| *overlap >= 0.20)
                .max_by(|left, right| left.1.total_cmp(&right.1));
            let track_id = best.map(|candidate| candidate.0).unwrap_or_else(|| {
                let id = self.next_id;
                self.next_id += 1;
                id
            });
            seen.insert(track_id);
            let mut rendered = object_detection_for_render(person);
            rendered.track_id = Some(track_id);
            rendered.stabilized = true;
            let measured = rendered.bounds();
            if let Some(state) = self.states.get_mut(&track_id) {
                let (old_cx, old_cy) = state.bounds.center();
                let (new_cx, new_cy) = measured.center();
                let old_width = state.bounds.width();
                let old_height = state.bounds.height();
                let mut delta_x = new_cx - old_cx;
                let mut delta_y = new_cy - old_cy;
                let distance = delta_x.hypot(delta_y);
                let max_step = old_width.max(old_height).mul_add(0.45, 0.0).max(18.0);
                if distance > max_step {
                    delta_x *= max_step / distance;
                    delta_y *= max_step / distance;
                }
                let center_x = old_cx + self.smoothing * delta_x;
                let center_y = old_cy + self.smoothing * delta_y;
                let size_smoothing = (self.smoothing * 0.65).clamp(0.05, 1.0);
                let box_width = old_width + size_smoothing * (measured.width() - old_width);
                let box_height = old_height + size_smoothing * (measured.height() - old_height);
                state.velocity_x = state.velocity_x * 0.65 + (center_x - old_cx) * 0.35;
                state.velocity_y = state.velocity_y * 0.65 + (center_y - old_cy) * 0.35;
                state.bounds = rect_from_center(center_x, center_y, box_width, box_height).clamp(width, height);
                state.template = rendered.clone();
                state.missed = 0;
                rendered.set_bounds(state.bounds, width, height);
            } else {
                self.states.insert(
                    track_id,
                    BodyState {
                        template: rendered.clone(),
                        bounds: measured,
                        velocity_x: 0.0,
                        velocity_y: 0.0,
                        missed: 0,
                    },
                );
            }
            output.push(rendered);
        }

        let live_people: Vec<crate::vision::Rect> = output
            .iter()
            .filter(|detection| detection.kind == "person" && !detection.held)
            .map(TrackedDetection::bounds)
            .collect();
        let mut expired = Vec::new();
        for (track_id, state) in &mut self.states {
            if seen.contains(track_id) {
                continue;
            }
            state.missed += 1;
            if state.missed > self.hold_frames {
                expired.push(*track_id);
                continue;
            }
            let (center_x, center_y) = state.bounds.center();
            state.bounds = rect_from_center(
                center_x + state.velocity_x * 0.65,
                center_y + state.velocity_y * 0.65,
                state.bounds.width(),
                state.bounds.height(),
            )
            .clamp(width, height);
            state.velocity_x *= 0.70;
            state.velocity_y *= 0.70;
            if live_people
                .iter()
                .any(|bounds| crate::vision::intersection_over_union(*bounds, state.bounds) > 0.35)
            {
                continue;
            }
            let mut held = state.template.clone();
            held.track_id = Some(*track_id);
            held.confidence *= 0.88f32.powi(state.missed as i32);
            held.source = "rust_temporal_body_hold".to_string();
            held.held = true;
            held.stabilized = true;
            held.set_bounds(state.bounds, width, height);
            output.push(held);
        }
        for track_id in expired {
            self.states.remove(&track_id);
        }
        output
    }
}

#[derive(Debug, Clone)]
struct FaceState {
    template: TrackedDetection,
    relative: [f32; 4],
    missed: usize,
}

struct FaceTracker {
    states: std::collections::HashMap<u32, FaceState>,
    smoothing: f32,
    hold_frames: usize,
}

impl FaceTracker {
    fn new(smoothing: f32, hold_frames: usize) -> Self {
        Self {
            states: std::collections::HashMap::new(),
            smoothing: smoothing.clamp(0.05, 1.0),
            hold_frames,
        }
    }

    fn update(
        &mut self,
        faces: Vec<crate::vision::FaceDetection>,
        detections: &[TrackedDetection],
        width: u32,
        height: u32,
    ) -> Vec<TrackedDetection> {
        let people: Vec<&TrackedDetection> = detections
            .iter()
            .filter(|detection| detection.kind == "person" && detection.track_id.is_some())
            .collect();
        let mut best_by_track: std::collections::HashMap<u32, crate::vision::FaceDetection> =
            std::collections::HashMap::new();
        for face in faces {
            let Some(person) = people.iter().copied().find(|person| plausible_face(person.bounds(), face.bounds)) else {
                continue;
            };
            let track_id = person.track_id.unwrap();
            if best_by_track
                .get(&track_id)
                .is_none_or(|current| face.confidence > current.confidence)
            {
                best_by_track.insert(track_id, face);
            }
        }
        let mut output = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for person in &people {
            let track_id = person.track_id.unwrap();
            let Some(face) = best_by_track.remove(&track_id) else {
                continue;
            };
            seen.insert(track_id);
            let measured = relative_rect(face.bounds, person.bounds());
            let mut rendered = TrackedDetection {
                track_id: Some(track_id),
                kind: "face".to_string(),
                label: "face/head".to_string(),
                confidence: face.confidence,
                x1: 0,
                y1: 0,
                x2: 0,
                y2: 0,
                source: "rust_yunet".to_string(),
                stabilized: true,
                held: false,
            };
            let state = self.states.entry(track_id).or_insert_with(|| FaceState {
                template: rendered.clone(),
                relative: measured,
                missed: 0,
            });
            let size_smoothing = (self.smoothing * 0.70).clamp(0.05, 1.0);
            state.relative[0] += self.smoothing * (measured[0] - state.relative[0]).clamp(-0.12, 0.12);
            state.relative[1] += self.smoothing * (measured[1] - state.relative[1]).clamp(-0.12, 0.12);
            state.relative[2] += size_smoothing * (measured[2] - state.relative[2]);
            state.relative[3] += size_smoothing * (measured[3] - state.relative[3]);
            state.template = rendered.clone();
            state.missed = 0;
            rendered.set_bounds(absolute_rect(state.relative, person.bounds()), width, height);
            output.push(rendered);
        }
        let mut expired = Vec::new();
        for (track_id, state) in &mut self.states {
            if seen.contains(track_id) {
                continue;
            }
            state.missed += 1;
            let Some(person) = people.iter().copied().find(|person| person.track_id == Some(*track_id)) else {
                expired.push(*track_id);
                continue;
            };
            if state.missed > self.hold_frames {
                expired.push(*track_id);
                continue;
            }
            let mut held = state.template.clone();
            held.confidence *= 0.86f32.powi(state.missed as i32);
            held.source = "rust_temporal_face_hold".to_string();
            held.held = true;
            held.set_bounds(absolute_rect(state.relative, person.bounds()), width, height);
            output.push(held);
        }
        for track_id in expired {
            self.states.remove(&track_id);
        }
        output
    }
}

fn lock_mp4_in_rust(
    input_path: &Path,
    output_path: &Path,
    tracks_path: &Path,
    threat_level: TrackerThreatLevel,
    concerning_behavior: bool,
    trusted_admin: bool,
) -> Result<(), String> {
    let raw_path = raw_video_path_for_mp4(input_path).ok_or_else(|| "cannot resolve recorder stream".to_string())?;
    let raw = fs::read(&raw_path).map_err(|error| format!("read {}: {error}", raw_path.display()))?;
    let frames = parse_frames(&raw).map_err(|error| format!("parse recorder frames: {error:?}"))?;
    let fps = average_fps(&frames).unwrap_or(10.0).clamp(1.0, 30.0);
    let rotation = choose_rust_frame_rotation(&frames)?;
    let object_confidence = env_f32("FRIDAY_YOLO_CONF", 0.08).clamp(0.01, 0.95);
    let body_smoothing = env_f32("FRIDAY_BODY_SMOOTHING", 0.30);
    let face_smoothing = env_f32("FRIDAY_FACE_SMOOTHING", 0.22);
    let hold_frames = env_usize("FRIDAY_TRACK_HOLD_FRAMES", 5).min(60);
    let threat_preroll = env_usize("FRIDAY_THREAT_PREROLL_FRAMES", 18).min(120);
    let threat_hold = env_usize("FRIDAY_THREAT_HOLD_FRAMES", 40).min(300);
    let mut body_tracker = BodyTracker::new(body_smoothing, hold_frames);
    let mut face_tracker = FaceTracker::new(face_smoothing, hold_frames);
    let mut records = Vec::with_capacity(frames.len());
    println!(
        "video: Rust tracker analyzing {} frame(s), rotation={rotation}, threshold={object_confidence:.2}",
        frames.len()
    );
    for (frame_index, frame) in frames.iter().enumerate() {
        let decoded = image::load_from_memory(frame.jpeg)
            .map_err(|error| format!("decode frame {frame_index}: {error}"))?;
        let rotated = crate::vision::rotate(&decoded, rotation);
        let mut detections = body_tracker.update(
            crate::vision::detect_objects(&rotated, object_confidence)?,
            rotated.width(),
            rotated.height(),
        );
        let faces = crate::vision::detect_faces(&rotated, 0.45)?;
        let tracked_faces = face_tracker.update(faces, &detections, rotated.width(), rotated.height());
        detections.extend(tracked_faces);
        records.push(TrackedFrame {
            frame_index,
            timestamp_ms: frame.timestamp_ms,
            event_threat_level: threat_level.as_str().to_string(),
            rendered_threat_level: "normal".to_string(),
            threat_preroll: false,
            detections,
        });
    }

    let mut threat_frames = vec![false; records.len()];
    for index in 0..records.len() {
        let has_object = records[index]
            .detections
            .iter()
            .any(|detection| detection.kind == "knife" || detection.kind == "object");
        if has_object {
            let start = index.saturating_sub(threat_preroll);
            let end = (index + threat_hold + 1).min(records.len());
            threat_frames[start..end].fill(true);
        }
    }
    for (index, record) in records.iter_mut().enumerate() {
        record.threat_preroll = threat_frames[index]
            && !record
                .detections
                .iter()
                .any(|detection| detection.kind == "knife" || detection.kind == "object");
        record.rendered_threat_level = if trusted_admin {
            "trusted".to_string()
        } else if threat_frames[index] {
            "threat".to_string()
        } else if concerning_behavior || threat_level == TrackerThreatLevel::Minimal {
            "minimal".to_string()
        } else {
            "normal".to_string()
        };
    }

    let parent = output_path.parent().ok_or_else(|| "locked video has no parent".to_string())?;
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let temporary_dir = parent.join(format!(".tmp_rust_tracker_{unique}"));
    fs::create_dir_all(&temporary_dir)
        .map_err(|error| format!("create {}: {error}", temporary_dir.display()))?;
    let render_result = (|| {
        for (frame, record) in frames.iter().zip(&records) {
            let decoded = image::load_from_memory(frame.jpeg)
                .map_err(|error| format!("decode render frame {}: {error}", record.frame_index))?;
            let mut rendered = crate::vision::rotate(&decoded, rotation).to_rgb8();
            draw_status(&mut rendered, &record.rendered_threat_level, trusted_admin);
            for detection in &record.detections {
                draw_detection(&mut rendered, detection, &record.rendered_threat_level, trusted_admin);
            }
            let path = temporary_dir.join(format!("frame_{:05}.jpg", record.frame_index));
            let file = fs::File::create(&path).map_err(|error| format!("create {}: {error}", path.display()))?;
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(file, 90);
            encoder
                .encode_image(&rendered)
                .map_err(|error| format!("write {}: {error}", path.display()))?;
        }
        encode_rendered_frames(&temporary_dir, output_path, fps)?;
        let metadata = serde_json::json!({
            "schema": "friday.tracks.v2",
            "engine": "rust-tract-onnx",
            "source_video": input_path,
            "object_model": "scripts/models/yolo11n.onnx",
            "face_model": "scripts/models/face_detection_yunet_2023mar.onnx",
            "tracker": "rust-iou-ema",
            "event_threat_level": threat_level.as_str(),
            "render_policy": "rust-stable-lock-v1",
            "concerning_behavior": concerning_behavior,
            "trusted_admin": trusted_admin,
            "gesture_policy": "event-analysis-backfill",
            "fps": fps,
            "frame_rotation": rotation,
            "frame_count": records.len(),
            "detection_count": records.iter().map(|record| record.detections.len()).sum::<usize>(),
            "frames": records,
        });
        let temporary_tracks = tracks_path.with_extension("json.tmp");
        fs::write(&temporary_tracks, serde_json::to_vec_pretty(&metadata).map_err(|error| error.to_string())?)
            .map_err(|error| format!("write {}: {error}", temporary_tracks.display()))?;
        fs::rename(&temporary_tracks, tracks_path)
            .map_err(|error| format!("rename {}: {error}", tracks_path.display()))?;
        Ok::<(), String>(())
    })();
    let _ = fs::remove_dir_all(&temporary_dir);
    render_result?;
    println!(
        "video: Rust tracker wrote {} and {} ({} frame(s))",
        output_path.display(),
        tracks_path.display(),
        records.len()
    );
    Ok(())
}

fn raw_video_path_for_mp4(input_path: &Path) -> Option<PathBuf> {
    let name = input_path.file_name()?.to_str()?;
    let stem = name.strip_suffix("_video.mp4")?;
    Some(input_path.with_file_name(format!("{stem}_video.bin")))
}

fn choose_rust_frame_rotation(frames: &[Frame<'_>]) -> Result<u16, String> {
    let indexes: std::collections::BTreeSet<usize> = [15usize, 35, 55, 75, 90]
        .into_iter()
        .map(|percent| frames.len().saturating_sub(1) * percent / 100)
        .collect();
    let mut best = (0.0f32, 0u16);
    for index in indexes {
        let image = image::load_from_memory(frames[index].jpeg)
            .map_err(|error| format!("decode rotation sample: {error}"))?;
        if let Some(face) = crate::vision::best_face_across_rotations(&image, 0.45)?
            && face.quality > best.0
        {
            best = (face.quality, face.rotation);
        }
    }
    Ok(best.1)
}

fn object_detection_for_render(detection: crate::vision::ObjectDetection) -> TrackedDetection {
    let kind = match detection.kind {
        crate::vision::ObjectKind::Person => "person",
        crate::vision::ObjectKind::Knife => "knife",
        crate::vision::ObjectKind::ConcerningObject => "object",
        crate::vision::ObjectKind::Package => "package",
        crate::vision::ObjectKind::Vehicle => "vehicle",
        crate::vision::ObjectKind::Animal => "animal",
    };
    TrackedDetection {
        track_id: None,
        kind: kind.to_string(),
        label: detection.label.to_string(),
        confidence: detection.confidence,
        x1: detection.bounds.x1.round() as i32,
        y1: detection.bounds.y1.round() as i32,
        x2: detection.bounds.x2.round() as i32,
        y2: detection.bounds.y2.round() as i32,
        source: "rust_yolo11n".to_string(),
        stabilized: false,
        held: false,
    }
}

fn rect_from_center(center_x: f32, center_y: f32, width: f32, height: f32) -> crate::vision::Rect {
    crate::vision::Rect {
        x1: center_x - width * 0.5,
        y1: center_y - height * 0.5,
        x2: center_x + width * 0.5,
        y2: center_y + height * 0.5,
    }
}

fn intersection_over_smaller(left: crate::vision::Rect, right: crate::vision::Rect) -> f32 {
    let intersection = (left.x2.min(right.x2) - left.x1.max(right.x1)).max(0.0)
        * (left.y2.min(right.y2) - left.y1.max(right.y1)).max(0.0);
    intersection / left.area().min(right.area()).max(1.0)
}

fn plausible_face(person: crate::vision::Rect, face: crate::vision::Rect) -> bool {
    let (face_center_x, face_center_y) = face.center();
    let relative_center_y = (face_center_y - person.y1) / person.height().max(1.0);
    person.x1 <= face_center_x
        && face_center_x <= person.x2
        && person.y1 <= face_center_y
        && face_center_y <= person.y2
        && face.width() / person.width().max(1.0) <= 0.70
        && face.height() / person.height().max(1.0) <= 0.45
        && face.area() / person.area().max(1.0) <= 0.25
        && relative_center_y <= 0.45
}

fn relative_rect(face: crate::vision::Rect, person: crate::vision::Rect) -> [f32; 4] {
    let (face_x, face_y) = face.center();
    [
        (face_x - person.x1) / person.width().max(1.0),
        (face_y - person.y1) / person.height().max(1.0),
        face.width() / person.width().max(1.0),
        face.height() / person.height().max(1.0),
    ]
}

fn absolute_rect(relative: [f32; 4], person: crate::vision::Rect) -> crate::vision::Rect {
    rect_from_center(
        person.x1 + relative[0] * person.width(),
        person.y1 + relative[1] * person.height(),
        relative[2] * person.width(),
        relative[3] * person.height(),
    )
}

fn detection_color(kind: &str, level: &str, trusted_admin: bool) -> image::Rgb<u8> {
    if kind == "knife" || kind == "object" {
        return image::Rgb([255, 0, 0]);
    }
    if kind == "package" || (trusted_admin && (kind == "person" || kind == "face")) {
        return image::Rgb([0, 255, 80]);
    }
    if kind == "person" || kind == "face" {
        return match level {
            "threat" => image::Rgb([255, 0, 0]),
            "minimal" => image::Rgb([255, 220, 0]),
            _ => image::Rgb([255, 255, 255]),
        };
    }
    image::Rgb([255, 255, 255])
}

fn draw_status(image: &mut image::RgbImage, level: &str, trusted_admin: bool) {
    let text = if trusted_admin {
        "FRIDAY LOCK - KAZ / ADMIN"
    } else {
        match level {
            "threat" => "FRIDAY LOCK - THREAT",
            "minimal" => "FRIDAY LOCK - MINIMAL",
            _ => "FRIDAY LOCK",
        }
    };
    draw_text(image, 18, 18, text, image::Rgb([0, 255, 255]), 2);
}

fn draw_detection(image: &mut image::RgbImage, detection: &TrackedDetection, level: &str, trusted_admin: bool) {
    let color = detection_color(&detection.kind, level, trusted_admin);
    draw_rectangle(image, detection.x1, detection.y1, detection.x2, detection.y2, color, 3);
    let label = match detection.track_id {
        Some(track_id) => format!("{} #{track_id}", detection.label.to_ascii_uppercase()),
        None => detection.label.to_ascii_uppercase(),
    };
    draw_text(image, detection.x1.max(0) as u32, detection.y1.saturating_sub(20).max(0) as u32, &label, color, 2);
}

fn draw_rectangle(
    image: &mut image::RgbImage,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    color: image::Rgb<u8>,
    thickness: i32,
) {
    let max_x = image.width().saturating_sub(1) as i32;
    let max_y = image.height().saturating_sub(1) as i32;
    let (x1, x2) = (x1.clamp(0, max_x), x2.clamp(0, max_x));
    let (y1, y2) = (y1.clamp(0, max_y), y2.clamp(0, max_y));
    for offset in 0..thickness {
        let left = (x1 + offset).min(x2);
        let right = (x2 - offset).max(x1);
        let top = (y1 + offset).min(y2);
        let bottom = (y2 - offset).max(y1);
        for x in left..=right {
            image.put_pixel(x as u32, top as u32, color);
            image.put_pixel(x as u32, bottom as u32, color);
        }
        for y in top..=bottom {
            image.put_pixel(left as u32, y as u32, color);
            image.put_pixel(right as u32, y as u32, color);
        }
    }
}

fn draw_text(image: &mut image::RgbImage, x: u32, y: u32, text: &str, color: image::Rgb<u8>, scale: u32) {
    use font8x8::UnicodeFonts;
    for (character_index, character) in text.chars().enumerate() {
        let Some(glyph) = font8x8::BASIC_FONTS.get(character) else {
            continue;
        };
        let origin_x = x + character_index as u32 * 9 * scale;
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..8 {
                if bits & (1 << column) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let pixel_x = origin_x + column * scale + dx;
                        let pixel_y = y + row as u32 * scale + dy;
                        if pixel_x < image.width() && pixel_y < image.height() {
                            image.put_pixel(pixel_x, pixel_y, color);
                        }
                    }
                }
            }
        }
    }
}

fn encode_rendered_frames(directory: &Path, output_path: &Path, fps: f64) -> Result<(), String> {
    let file_name = output_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "invalid locked video name".to_string())?;
    let temporary_output = output_path.with_file_name(format!(".{file_name}.tmp.mp4"));
    let output = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-framerate")
        .arg(format!("{fps:.3}"))
        .arg("-i")
        .arg(directory.join("frame_%05d.jpg"))
        .arg("-c:v")
        .arg("libx264")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-movflags")
        .arg("+faststart")
        .arg("-an")
        .arg(&temporary_output)
        .output()
        .map_err(|error| format!("start ffmpeg: {error}"))?;
    if !output.status.success() {
        return Err(format!("ffmpeg failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    fs::rename(&temporary_output, output_path)
        .map_err(|error| format!("rename {}: {error}", output_path.display()))
}

fn env_f32(name: &str, default: f32) -> f32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
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
        .map(|level| format!("_locked_rust_{}", level.as_str()))
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
        .map(|level| format!("_tracks_rust_{}", level.as_str()))
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

    fn person(bounds: crate::vision::Rect) -> crate::vision::ObjectDetection {
        crate::vision::ObjectDetection {
            bounds,
            kind: crate::vision::ObjectKind::Person,
            label: "person",
            confidence: 0.9,
        }
    }

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
    fn rust_tracker_outputs_use_a_fresh_cache_name() {
        let input = Path::new("/tmp/event_123_video.mp4");

        assert_eq!(
            locked_output_path(input, Some(TrackerThreatLevel::Normal)),
            Some(PathBuf::from("/tmp/event_123_locked_rust_normal.mp4"))
        );
        assert_eq!(
            tracks_output_path(input, Some(TrackerThreatLevel::Threat)),
            Some(PathBuf::from("/tmp/event_123_tracks_rust_threat.json"))
        );
    }

    #[test]
    fn rust_body_tracker_smooths_and_keeps_the_same_id() {
        let mut tracker = BodyTracker::new(0.30, 5);
        let first = tracker.update(
            vec![person(crate::vision::Rect {
                x1: 0.0,
                y1: 0.0,
                x2: 100.0,
                y2: 200.0,
            })],
            640,
            480,
        );
        let second = tracker.update(
            vec![person(crate::vision::Rect {
                x1: 20.0,
                y1: 0.0,
                x2: 120.0,
                y2: 200.0,
            })],
            640,
            480,
        );
        assert_eq!(first[0].track_id, Some(1));
        assert_eq!(second[0].track_id, Some(1));
        assert!(second[0].x1 > first[0].x1);
        assert!(second[0].x1 < 20, "raw jump should be smoothed");
    }

    #[test]
    fn rust_body_tracker_holds_one_short_miss_then_expires() {
        let mut tracker = BodyTracker::new(0.30, 1);
        tracker.update(
            vec![person(crate::vision::Rect {
                x1: 10.0,
                y1: 10.0,
                x2: 110.0,
                y2: 210.0,
            })],
            640,
            480,
        );
        let held = tracker.update(Vec::new(), 640, 480);
        assert_eq!(held.len(), 1);
        assert!(held[0].held);
        assert!(tracker.update(Vec::new(), 640, 480).is_empty());
    }

    #[test]
    fn trusted_admin_person_color_is_green_even_at_threat_level() {
        assert_eq!(
            detection_color("person", "threat", true),
            image::Rgb([0, 255, 80])
        );
        assert_eq!(
            detection_color("knife", "trusted", true),
            image::Rgb([255, 0, 0])
        );
    }

    #[test]
    fn face_must_be_small_and_in_the_upper_body() {
        let person = crate::vision::Rect {
            x1: 100.0,
            y1: 20.0,
            x2: 500.0,
            y2: 470.0,
        };
        assert!(plausible_face(
            person,
            crate::vision::Rect {
                x1: 260.0,
                y1: 45.0,
                x2: 350.0,
                y2: 145.0,
            }
        ));
        assert!(!plausible_face(
            person,
            crate::vision::Rect {
                x1: 180.0,
                y1: 180.0,
                x2: 430.0,
                y2: 430.0,
            }
        ));
    }
}
