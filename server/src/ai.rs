//! Server-side AI analysis of a whole camera event, via a local Ollama
//! vision model (default `qwen3.5:4b`): the thumbnail plus any extracted
//! video key frames (`video::extract_keyframes`) are sent together as one
//! multi-image request, so the model describes the event as a whole
//! rather than just its first still. Still no *raw video* analysis --
//! Ollama is never given the `.bin` file itself, only the JPEG stills
//! already extracted from it (see `video.rs`); if extraction produced
//! nothing (no video part, extraction failed, or the clip was empty),
//! this falls back to thumbnail-only analysis exactly like before.
//!
//! Runs strictly *after* the event is already fully stored and the
//! firmware upload has already been answered with `200`, on a detached
//! background thread (see the call site in `main.rs`) -- this server's
//! accept loop is single-threaded and synchronous, so an in-line call
//! here would block every other upload from being accepted until Ollama
//! finished, not just the current one. `analyze_and_save` itself never
//! returns an error; every failure just downgrades `ai.status` to
//! `"failed"` and moves on.
//!
//! The saved `analysis.json` also carries an `identity` block that is
//! always `"not_enabled"` today -- it exists purely so a future local
//! known-person-recognition module can be plugged in later (matched against
//! a face embedding, a local people database, etc.) without changing this
//! file's shape or the event storage pipeline again. Nothing here infers,
//! stores, or asks the model for anyone's identity.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Fixed prompt sent to the vision model for every event -- the first
/// image is always the thumbnail, any remaining images are video key
/// frames in chronological order. Deliberately security-focused and
/// explicit about *not* identifying anyone or describing sensitive
/// appearance traits, since this is a door-camera event a person may not
/// know is being analyzed.
const PROMPT: &str = r#"Analyze this smart door-camera event using the thumbnail and video keyframes provided.

Return JSON only with exactly these fields:
{
  "person": boolean,
  "package": boolean,
  "vehicle": boolean,
  "animal": boolean,
  "description": string,
  "importance": "low" | "medium" | "high"
}

Rules:
- Treat all provided images as one event over time.
- The first image is the event thumbnail.
- The remaining images are keyframes from the recorded video.
- Do not identify anyone.
- Do not describe race, ethnicity, gender, age, attractiveness, or other sensitive appearance traits.
- Do not call the image a selfie.
- Keep the description neutral, short, and security-focused.
- If a person appears in any frame, set person to true.
- If a package appears in any frame, set package to true.
- If a vehicle appears in any frame, set vehicle to true.
- If an animal appears in any frame, set animal to true.
- Set importance to high if a person or package appears near the door/camera.
- Set importance to medium if a person/vehicle/animal appears but there is no clear concern.
- Set importance to low if none of the tracked objects appear."#;

/// One event's AI-derived analysis -- exactly the fields the vision model
/// is asked to return. Parsed strictly: a response that doesn't match this
/// shape is treated as a failure, not silently patched or defaulted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventAnalysis {
    pub person: bool,
    pub package: bool,
    pub vehicle: bool,
    pub animal: bool,
    pub description: String,
    pub importance: Importance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Importance {
    Low,
    Medium,
    High,
}

/// Placeholder for a future known-person-recognition module. Deliberately
/// inert: no face recognition, no embeddings, no people database exist yet,
/// and the vision model is never asked to identify anyone -- `status` is
/// always `"not_enabled"` until that future module exists and sets it.
#[derive(Debug, Clone, Serialize)]
pub struct Identity {
    pub status: &'static str,
    pub known_person_id: Option<String>,
    pub display_name: Option<String>,
    pub confidence: Option<f32>,
}

impl Identity {
    fn not_enabled() -> Self {
        Identity {
            status: "not_enabled",
            known_person_id: None,
            display_name: None,
            confidence: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AiStatus {
    Success,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiMeta {
    pub provider: String,
    pub model: String,
    pub status: AiStatus,
}

/// The full `analysis.json` shape saved beside an event's stored files.
/// `event_analysis` is `None` (serialized as `null`) whenever analysis
/// failed for any reason -- the key is always present either way, so
/// consumers don't need to distinguish "field missing" from "no result".
#[derive(Debug, Clone, Serialize)]
pub struct AnalysisResult {
    pub event_analysis: Option<EventAnalysis>,
    pub identity: Identity,
    pub ai: AiMeta,
}

#[derive(Debug)]
pub enum AnalyzeError {
    /// Fields are only ever read via this enum's own `Debug` impl (the
    /// `println!("...{e:?}")` log line in `analyze_and_save`), which the
    /// dead-code lint doesn't credit as a use -- same caveat `main.rs`'s
    /// `RequestError` variants document.
    #[allow(dead_code)]
    Request(String),
    #[allow(dead_code)]
    InvalidResponse(String),
}

/// Anything that can turn a set of JPEG images (thumbnail first, then any
/// video key frames in order) into one `EventAnalysis` for the whole
/// event. `OllamaAnalyzer` is the real implementation; tests use a fake
/// one so they never require Ollama to actually be running.
/// `provider`/`model` feed directly into the saved `ai` block, so each
/// implementation is the single source of truth for what it actually
/// is/uses.
///
/// `Send + Sync` because `main.rs` runs `analyze_and_save` on a detached
/// background thread per event (see its doc comment) -- this server's
/// accept loop is single-threaded/synchronous, so a slow analysis call must
/// never run inline with request handling, or it would block every other
/// upload from even being accepted until it finished.
pub trait EventAnalyzer: Send + Sync {
    fn analyze(&self, images: &[&[u8]]) -> Result<EventAnalysis, AnalyzeError>;
    fn provider(&self) -> &str;
    fn model(&self) -> &str;
}

/// Calls a local Ollama server's `/api/generate` endpoint with every image
/// (thumbnail first, then key frames) base64-encoded into one request and
/// the fixed prompt above. Constructing this never talks to Ollama -- the
/// server must be able to start (and keep serving uploads) with Ollama
/// offline; only an actual `analyze` call touches the network, and that
/// call is always bounded by `timeout`.
pub struct OllamaAnalyzer {
    base_url: String,
    model: String,
    timeout: Duration,
}

impl OllamaAnalyzer {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>, timeout: Duration) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            timeout,
        }
    }

    /// Reads `OLLAMA_URL`/`OLLAMA_MODEL` env vars, falling back to
    /// `http://localhost:11434`/`qwen3.5:4b` when unset.
    pub fn from_env(timeout: Duration) -> Self {
        let base_url = std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".to_string());
        let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3.5:4b".to_string());
        Self::new(base_url, model, timeout)
    }
}

#[derive(Serialize)]
struct OllamaGenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    images: Vec<String>,
    stream: bool,
    format: &'a str,
    /// Explicitly disabled: hybrid reasoning models (e.g. `qwen3.5:4b`, which
    /// this server defaults to) otherwise put their entire answer in
    /// Ollama's separate `thinking` field and leave `response` empty --
    /// confirmed directly against a running Ollama instance during
    /// development (see PROJECT_STATUS.md). Also meaningfully faster with
    /// it off, since the model skips its chain-of-thought pass entirely.
    think: bool,
}

#[derive(Deserialize)]
struct OllamaGenerateResponse {
    response: String,
}

impl EventAnalyzer for OllamaAnalyzer {
    fn provider(&self) -> &str {
        "ollama"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn analyze(&self, images: &[&[u8]]) -> Result<EventAnalysis, AnalyzeError> {
        let images_b64: Vec<String> = images.iter().map(|img| STANDARD.encode(img)).collect();
        let request = OllamaGenerateRequest {
            model: &self.model,
            prompt: PROMPT,
            images: images_b64,
            stream: false,
            format: "json",
            think: false,
        };

        let url = format!("{}/api/generate", self.base_url.trim_end_matches('/'));
        let mut response = ureq::post(url.as_str())
            .config()
            .timeout_global(Some(self.timeout))
            .build()
            .send_json(&request)
            .map_err(|e| AnalyzeError::Request(e.to_string()))?;

        let parsed: OllamaGenerateResponse = response
            .body_mut()
            .read_json()
            .map_err(|e| AnalyzeError::InvalidResponse(e.to_string()))?;

        parse_event_analysis(&parsed.response)
    }
}

/// Extracts the strict `EventAnalysis` JSON from the model's raw text
/// response. Tolerates the model wrapping otherwise-valid JSON in a
/// markdown code fence despite being asked for JSON only (a common,
/// harmless vision-model quirk); anything else unparseable is a real
/// failure, not silently patched.
fn parse_event_analysis(raw: &str) -> Result<EventAnalysis, AnalyzeError> {
    let trimmed = raw.trim();
    let json_text = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.strip_suffix("```").unwrap_or(s))
        .unwrap_or(trimmed)
        .trim();

    serde_json::from_str(json_text).map_err(|e| AnalyzeError::InvalidResponse(format!("{e}: {json_text:?}")))
}

/// Reads the thumbnail plus any given key frames (in order), runs them
/// through `analyzer` as one multi-image event, and writes `analysis.json`
/// beside the thumbnail. Never returns an error or panics -- see the
/// module doc for why AI analysis must never be able to affect whether an
/// event upload succeeds.
///
/// The thumbnail is required -- if it can't be read, analysis fails
/// entirely, same as before this milestone. Key frames are best-effort
/// supplements: any individual key frame that can't be read is skipped
/// (logged, not fatal), and if `keyframe_paths` is empty or every one of
/// them fails to read, this transparently falls back to exactly the
/// thumbnail-only analysis Milestone 19 already did -- there is no
/// separate "fallback mode", just fewer images in the same request.
pub fn analyze_and_save(
    analyzer: &dyn EventAnalyzer,
    thumbnail_path: &Path,
    keyframe_paths: &[PathBuf],
    analysis_path: &Path,
) {
    let meta = |status| AiMeta {
        provider: analyzer.provider().to_string(),
        model: analyzer.model().to_string(),
        status,
    };

    let thumbnail_bytes = match fs::read(thumbnail_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            println!("AI analysis: failed to read thumbnail {}: {e}", thumbnail_path.display());
            let result = AnalysisResult {
                event_analysis: None,
                identity: Identity::not_enabled(),
                ai: meta(AiStatus::Failed),
            };
            if let Err(e) = save_analysis_json(analysis_path, &result) {
                println!("AI analysis: failed to save {}: {e}", analysis_path.display());
            }
            return;
        }
    };

    let mut images = vec![thumbnail_bytes];
    for keyframe_path in keyframe_paths {
        match fs::read(keyframe_path) {
            Ok(bytes) => images.push(bytes),
            Err(e) => println!("AI analysis: failed to read keyframe {}: {e} (skipping it)", keyframe_path.display()),
        }
    }

    let image_refs: Vec<&[u8]> = images.iter().map(|b| b.as_slice()).collect();
    let result = match analyzer.analyze(&image_refs) {
        Ok(event_analysis) => AnalysisResult {
            event_analysis: Some(event_analysis),
            identity: Identity::not_enabled(),
            ai: meta(AiStatus::Success),
        },
        Err(e) => {
            println!("AI analysis failed for {} ({} image(s)): {e:?}", thumbnail_path.display(), image_refs.len());
            AnalysisResult {
                event_analysis: None,
                identity: Identity::not_enabled(),
                ai: meta(AiStatus::Failed),
            }
        }
    };

    if let Err(e) = save_analysis_json(analysis_path, &result) {
        println!("AI analysis: failed to save {}: {e}", analysis_path.display());
    }
}

/// Writes `analysis.json` via the same temp-file-then-rename pattern
/// `commit_part_file` uses for event media, so a crash mid-write can never
/// leave a half-written analysis file behind.
fn save_analysis_json(path: &Path, result: &AnalysisResult) -> io::Result<()> {
    let contents = serde_json::to_string_pretty(result).expect("AnalysisResult always serializes");
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, contents)?;
    fs::rename(&tmp_path, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct FakeAnalyzer {
        result: Result<EventAnalysis, ()>,
    }

    impl EventAnalyzer for FakeAnalyzer {
        fn provider(&self) -> &str {
            "ollama"
        }

        fn model(&self) -> &str {
            "qwen3.5:4b"
        }

        fn analyze(&self, _images: &[&[u8]]) -> Result<EventAnalysis, AnalyzeError> {
            self.result
                .clone()
                .map_err(|()| AnalyzeError::Request("fake failure".to_string()))
        }
    }

    /// Records exactly which images (as byte vectors, so tests can assert
    /// on both count and content) it was called with, so tests can prove
    /// what `analyze_and_save` actually sent to the analyzer -- not just
    /// that *something* was saved.
    struct RecordingAnalyzer {
        received: Mutex<Vec<Vec<Vec<u8>>>>,
        result: Result<EventAnalysis, ()>,
    }

    impl RecordingAnalyzer {
        fn new(result: Result<EventAnalysis, ()>) -> Self {
            Self {
                received: Mutex::new(Vec::new()),
                result,
            }
        }

        fn calls(&self) -> Vec<Vec<Vec<u8>>> {
            self.received.lock().unwrap().clone()
        }
    }

    impl EventAnalyzer for RecordingAnalyzer {
        fn provider(&self) -> &str {
            "ollama"
        }

        fn model(&self) -> &str {
            "qwen3.5:4b"
        }

        fn analyze(&self, images: &[&[u8]]) -> Result<EventAnalysis, AnalyzeError> {
            self.received.lock().unwrap().push(images.iter().map(|b| b.to_vec()).collect());
            self.result
                .clone()
                .map_err(|()| AnalyzeError::Request("fake failure".to_string()))
        }
    }

    fn sample_analysis() -> EventAnalysis {
        EventAnalysis {
            person: true,
            package: false,
            vehicle: false,
            animal: false,
            description: "A person is near the camera.".to_string(),
            importance: Importance::Medium,
        }
    }

    fn scratch_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let dir = std::env::temp_dir().join(format!("camera_server_test_ai_{label}_{unique}"));
        fs::create_dir_all(&dir).expect("scratch dir should be creatable");
        dir
    }

    fn scratch_paths(label: &str) -> (PathBuf, PathBuf) {
        let dir = scratch_dir(label);
        (dir.join("thumbnail.jpg"), dir.join("analysis.json"))
    }

    #[test]
    fn saves_success_shape_when_analysis_succeeds() {
        let (thumb_path, analysis_path) = scratch_paths("success");
        fs::write(&thumb_path, b"fake-jpeg-bytes").unwrap();

        let analyzer = FakeAnalyzer {
            result: Ok(sample_analysis()),
        };
        analyze_and_save(&analyzer, &thumb_path, &[], &analysis_path);

        let saved: serde_json::Value = serde_json::from_str(&fs::read_to_string(&analysis_path).unwrap()).unwrap();
        assert_eq!(saved["event_analysis"]["person"], true);
        assert_eq!(saved["event_analysis"]["importance"], "medium");
        assert_eq!(saved["identity"]["status"], "not_enabled");
        assert!(saved["identity"]["known_person_id"].is_null());
        assert_eq!(saved["ai"]["provider"], "ollama");
        assert_eq!(saved["ai"]["model"], "qwen3.5:4b");
        assert_eq!(saved["ai"]["status"], "success");

        fs::remove_dir_all(thumb_path.parent().unwrap()).ok();
    }

    #[test]
    fn saves_failed_shape_when_analyzer_errors_but_never_panics() {
        let (thumb_path, analysis_path) = scratch_paths("analyzer_error");
        fs::write(&thumb_path, b"fake-jpeg-bytes").unwrap();

        let analyzer = FakeAnalyzer { result: Err(()) };
        analyze_and_save(&analyzer, &thumb_path, &[], &analysis_path);

        let saved: serde_json::Value = serde_json::from_str(&fs::read_to_string(&analysis_path).unwrap()).unwrap();
        assert!(saved["event_analysis"].is_null());
        assert_eq!(saved["identity"]["status"], "not_enabled");
        assert_eq!(saved["ai"]["status"], "failed");

        fs::remove_dir_all(thumb_path.parent().unwrap()).ok();
    }

    #[test]
    fn saves_failed_shape_when_thumbnail_is_missing() {
        let (thumb_path, analysis_path) = scratch_paths("missing_thumbnail");
        // Deliberately never written -- simulates a thumbnail that
        // vanished or was never stored.

        let analyzer = FakeAnalyzer {
            result: Ok(sample_analysis()),
        };
        analyze_and_save(&analyzer, &thumb_path, &[], &analysis_path);

        let saved: serde_json::Value = serde_json::from_str(&fs::read_to_string(&analysis_path).unwrap()).unwrap();
        assert!(saved["event_analysis"].is_null());
        assert_eq!(saved["ai"]["status"], "failed");

        fs::remove_dir_all(analysis_path.parent().unwrap()).ok();
    }

    #[test]
    fn thumbnail_and_keyframes_are_all_passed_to_the_analyzer_in_order() {
        let dir = scratch_dir("multi_image");
        let thumb_path = dir.join("event_1_thumbnail.jpg");
        let analysis_path = dir.join("event_1_analysis.json");
        fs::write(&thumb_path, b"thumbnail-bytes").unwrap();

        let keyframe_paths: Vec<PathBuf> = (0..3)
            .map(|i| {
                let path = dir.join(format!("event_1_keyframe_{i}.jpg"));
                fs::write(&path, format!("keyframe-{i}-bytes")).unwrap();
                path
            })
            .collect();

        let analyzer = RecordingAnalyzer::new(Ok(sample_analysis()));
        analyze_and_save(&analyzer, &thumb_path, &keyframe_paths, &analysis_path);

        let calls = analyzer.calls();
        assert_eq!(calls.len(), 1, "analyze should be called exactly once");
        let images = &calls[0];
        assert_eq!(images.len(), 4, "thumbnail + 3 keyframes");
        assert_eq!(images[0], b"thumbnail-bytes", "thumbnail must be first");
        assert_eq!(images[1], b"keyframe-0-bytes");
        assert_eq!(images[2], b"keyframe-1-bytes");
        assert_eq!(images[3], b"keyframe-2-bytes");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_keyframes_falls_back_to_thumbnail_only_safely() {
        let dir = scratch_dir("missing_keyframes");
        let thumb_path = dir.join("event_2_thumbnail.jpg");
        let analysis_path = dir.join("event_2_analysis.json");
        fs::write(&thumb_path, b"thumbnail-bytes").unwrap();

        // Neither keyframe file actually exists on disk -- simulates
        // extraction failing or producing nothing.
        let keyframe_paths = vec![
            dir.join("event_2_keyframe_0.jpg"),
            dir.join("event_2_keyframe_1.jpg"),
        ];

        let analyzer = RecordingAnalyzer::new(Ok(sample_analysis()));
        analyze_and_save(&analyzer, &thumb_path, &keyframe_paths, &analysis_path);

        let calls = analyzer.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 1, "only the thumbnail should have been sent");
        assert_eq!(calls[0][0], b"thumbnail-bytes");

        // Still produces a normal, successful analysis.json -- a missing
        // keyframe is not a failure, just fewer images in the request.
        let saved: serde_json::Value = serde_json::from_str(&fs::read_to_string(&analysis_path).unwrap()).unwrap();
        assert_eq!(saved["ai"]["status"], "success");
        assert_eq!(saved["identity"]["status"], "not_enabled");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_keyframe_list_behaves_exactly_like_thumbnail_only() {
        let (thumb_path, analysis_path) = scratch_paths("empty_keyframes");
        fs::write(&thumb_path, b"thumbnail-bytes").unwrap();

        let analyzer = RecordingAnalyzer::new(Ok(sample_analysis()));
        analyze_and_save(&analyzer, &thumb_path, &[], &analysis_path);

        let calls = analyzer.calls();
        assert_eq!(calls[0].len(), 1);
        assert_eq!(calls[0][0], b"thumbnail-bytes");

        fs::remove_dir_all(thumb_path.parent().unwrap()).ok();
    }

    #[test]
    fn upload_semantics_still_succeed_when_ai_fails_with_keyframes_present() {
        // Named to make the acceptance criterion explicit: even with
        // keyframes in play, a failing analyzer must still result in a
        // saved (failed-shape) analysis.json, never a panic -- the caller
        // (main.rs's process_request) has already answered the firmware
        // upload with 200 by the time this ever runs.
        let dir = scratch_dir("fails_with_keyframes");
        let thumb_path = dir.join("event_3_thumbnail.jpg");
        let analysis_path = dir.join("event_3_analysis.json");
        let keyframe_path = dir.join("event_3_keyframe_0.jpg");
        fs::write(&thumb_path, b"thumbnail-bytes").unwrap();
        fs::write(&keyframe_path, b"keyframe-bytes").unwrap();

        let analyzer = FakeAnalyzer { result: Err(()) };
        analyze_and_save(&analyzer, &thumb_path, &[keyframe_path], &analysis_path);

        let saved: serde_json::Value = serde_json::from_str(&fs::read_to_string(&analysis_path).unwrap()).unwrap();
        assert!(saved["event_analysis"].is_null());
        assert_eq!(saved["ai"]["status"], "failed");
        assert_eq!(saved["identity"]["status"], "not_enabled");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_a_markdown_fenced_response() {
        let fenced = "```json\n{\"person\":false,\"package\":true,\"vehicle\":false,\"animal\":false,\"description\":\"A package is on the porch.\",\"importance\":\"high\"}\n```";
        let parsed = parse_event_analysis(fenced).expect("fenced JSON should still parse");
        assert_eq!(parsed.package, true);
        assert_eq!(parsed.importance, Importance::High);
    }

    #[test]
    fn rejects_a_response_missing_required_fields() {
        let bad = r#"{"person": true}"#;
        assert!(parse_event_analysis(bad).is_err());
    }

    #[test]
    fn identity_is_always_not_enabled_regardless_of_outcome() {
        assert_eq!(Identity::not_enabled().status, "not_enabled");
    }
}
