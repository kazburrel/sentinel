//! Local known-person recognition for stored FRIDAY events.
//!
//! Detection, alignment, SFace inference, and cosine matching all run in
//! this Rust process through the pure-Rust `tract` ONNX engine. Source
//! images remain local and recognition failure always fails open.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ai::Identity;
use crate::vision;

const PROFILE_VERSION: u8 = 1;
const DEFAULT_MATCH_THRESHOLD: f32 = 0.50;
const DEFAULT_STRONG_MATCH_THRESHOLD: f32 = 0.65;
const DEFAULT_FACE_THRESHOLD: f32 = 0.45;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdentityProfile {
    version: u8,
    person_id: String,
    display_name: String,
    model: String,
    model_sha256: String,
    created_at_unix: u64,
    embedding_count: usize,
    embeddings: Vec<Vec<f32>>,
}

pub fn recognize_event(thumbnail_path: &Path, keyframe_paths: &[PathBuf]) -> Identity {
    let profiles_dir = profiles_dir();
    if !has_profiles(&profiles_dir) {
        return Identity::not_enrolled();
    }
    match recognize_event_inner(thumbnail_path, keyframe_paths, &profiles_dir) {
        Ok(identity) => {
            match identity.display_name.as_deref() {
                Some(name) if identity.is_known() => println!(
                    "identity: recognized {name} ({:.1}% cosine confidence, Rust ONNX)",
                    identity.confidence.unwrap_or_default() * 100.0
                ),
                _ => println!("identity: {} (Rust ONNX)", identity.status),
            }
            identity
        }
        Err(error) => {
            println!("identity: Rust recognition failed open: {error}");
            Identity::failed()
        }
    }
}

fn recognize_event_inner(
    thumbnail_path: &Path,
    keyframe_paths: &[PathBuf],
    profiles_dir: &Path,
) -> Result<Identity, String> {
    let threshold = env_threshold("FRIDAY_FACE_MATCH_THRESHOLD", DEFAULT_MATCH_THRESHOLD);
    let strong_threshold = env_threshold(
        "FRIDAY_FACE_STRONG_MATCH_THRESHOLD",
        DEFAULT_STRONG_MATCH_THRESHOLD,
    );
    if !(0.0..=1.0).contains(&threshold) || !(threshold..=1.0).contains(&strong_threshold) {
        return Err("invalid face-match thresholds".to_string());
    }
    let recognition_model = vision::model_path("face_recognition_sface_2021dec.onnx");
    let model_sha256 = vision::model_sha256(&recognition_model)?;
    let profiles = load_profiles(profiles_dir, &model_sha256);
    if profiles.is_empty() {
        return Ok(Identity::not_enrolled());
    }
    let paths = std::iter::once(thumbnail_path).chain(keyframe_paths.iter().map(PathBuf::as_path));
    let (embeddings, multiple_faces) = extract_embeddings(paths, DEFAULT_FACE_THRESHOLD)?;
    if multiple_faces {
        return Ok(identity_status("multiple_faces", None));
    }
    if embeddings.is_empty() {
        return Ok(identity_status("no_face", None));
    }
    Ok(best_profile_match(
        &embeddings,
        &profiles,
        threshold,
        strong_threshold,
    ))
}

pub fn enroll_profile(
    person_id: &str,
    display_name: &str,
    output_path: &Path,
    image_paths: &[PathBuf],
) -> Result<usize, String> {
    if person_id.trim().is_empty() || display_name.trim().is_empty() {
        return Err("person ID and display name must not be empty".to_string());
    }
    let (embeddings, multiple_faces) = extract_embeddings(
        image_paths.iter().map(PathBuf::as_path),
        DEFAULT_FACE_THRESHOLD,
    )?;
    if multiple_faces {
        return Err("enrollment images must contain only the person being enrolled".to_string());
    }
    if embeddings.len() < 2 {
        return Err("enrollment needs usable faces from at least two images".to_string());
    }
    let model_path = vision::model_path("face_recognition_sface_2021dec.onnx");
    let profile = IdentityProfile {
        version: PROFILE_VERSION,
        person_id: person_id.trim().to_string(),
        display_name: display_name.trim().to_string(),
        model: model_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("face_recognition_sface_2021dec.onnx")
            .to_string(),
        model_sha256: vision::model_sha256(&model_path)?,
        created_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        embedding_count: embeddings.len(),
        embeddings,
    };
    write_private_profile(output_path, &profile)?;
    Ok(profile.embedding_count)
}

fn extract_embeddings<'a>(
    paths: impl Iterator<Item = &'a Path>,
    face_threshold: f32,
) -> Result<(Vec<Vec<f32>>, bool), String> {
    let mut embeddings = Vec::new();
    let mut multiple_faces = false;
    for path in paths {
        if !path.is_file() {
            println!("identity: missing image skipped: {}", path.display());
            continue;
        }
        let image = match image::open(path) {
            Ok(image) => image,
            Err(error) => {
                println!(
                    "identity: unreadable image skipped: {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        let Some(best_face) = vision::best_face_across_rotations(&image, face_threshold)? else {
            println!("identity: no usable face in {}", path.display());
            continue;
        };
        multiple_faces |= best_face.face_count > 1;
        embeddings.push(vision::face_embedding(
            &best_face.image,
            &best_face.detection,
        )?);
    }
    Ok((embeddings, multiple_faces))
}

fn best_profile_match(
    query_embeddings: &[Vec<f32>],
    profiles: &[IdentityProfile],
    threshold: f32,
    strong_threshold: f32,
) -> Identity {
    let mut best: Option<(usize, f32, f32, &IdentityProfile)> = None;
    let mut best_unknown_score = -1.0f32;
    for profile in profiles {
        let mut scores = Vec::with_capacity(query_embeddings.len());
        for query in query_embeddings {
            let score = profile
                .embeddings
                .iter()
                .filter_map(|reference| vision::cosine_similarity(query, reference).ok())
                .fold(-1.0f32, f32::max);
            scores.push(score);
        }
        scores.sort_by(|left, right| right.total_cmp(left));
        if let Some(strongest) = scores.first().copied() {
            best_unknown_score = best_unknown_score.max(strongest);
            let supporting: Vec<f32> = scores
                .iter()
                .copied()
                .filter(|score| *score >= threshold)
                .collect();
            let support_count = supporting.len();
            let confidence = if supporting.is_empty() {
                strongest
            } else {
                supporting.iter().take(2).sum::<f32>() / supporting.len().min(2) as f32
            };
            let candidate = (support_count, confidence, strongest, profile);
            if best.as_ref().is_none_or(|current| {
                candidate.0 > current.0
                    || (candidate.0 == current.0 && candidate.1 > current.1)
                    || (candidate.0 == current.0
                        && candidate.1 == current.1
                        && candidate.2 > current.2)
            }) {
                best = Some(candidate);
            }
        }
    }
    let Some((support_count, confidence, strongest, profile)) = best else {
        return identity_status("unknown", None);
    };
    if support_count >= 2 || strongest >= strong_threshold {
        Identity {
            status: "known".to_string(),
            known_person_id: Some(profile.person_id.clone()),
            display_name: Some(profile.display_name.clone()),
            confidence: Some(
                if support_count == 1 {
                    strongest
                } else {
                    confidence
                }
                .clamp(0.0, 1.0),
            ),
        }
    } else {
        identity_status("unknown", Some(best_unknown_score.clamp(0.0, 1.0)))
    }
}

fn identity_status(status: &str, confidence: Option<f32>) -> Identity {
    Identity {
        status: status.to_string(),
        known_person_id: None,
        display_name: None,
        confidence,
    }
}

fn load_profiles(directory: &Path, expected_model_sha256: &str) -> Vec<IdentityProfile> {
    let mut paths: Vec<PathBuf> = fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| match load_profile(&path, expected_model_sha256) {
            Ok(profile) => Some(profile),
            Err(error) => {
                println!(
                    "identity: invalid profile skipped: {}: {error}",
                    path.display()
                );
                None
            }
        })
        .collect()
}

fn load_profile(path: &Path, expected_model_sha256: &str) -> Result<IdentityProfile, String> {
    let contents = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut profile: IdentityProfile =
        serde_json::from_str(&contents).map_err(|error| error.to_string())?;
    if profile.version != PROFILE_VERSION {
        return Err("unsupported profile version".to_string());
    }
    if profile.model_sha256 != expected_model_sha256 {
        return Err("profile was created with a different recognition model".to_string());
    }
    if profile.person_id.trim().is_empty()
        || profile.display_name.trim().is_empty()
        || profile.embeddings.is_empty()
    {
        return Err("profile is incomplete".to_string());
    }
    for embedding in &mut profile.embeddings {
        *embedding = vision::normalize_embedding(std::mem::take(embedding))?;
    }
    profile.embedding_count = profile.embeddings.len();
    Ok(profile)
}

fn write_private_profile(path: &Path, profile: &IdentityProfile) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "identity profile path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    let temporary = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("identity")
    ));
    let json = serde_json::to_vec_pretty(profile).map_err(|error| error.to_string())?;
    fs::write(&temporary, [json.as_slice(), b"\n"].concat())
        .map_err(|error| format!("write {}: {error}", temporary.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("protect {}: {error}", temporary.display()))?;
    }
    fs::rename(&temporary, path).map_err(|error| {
        format!(
            "rename {} to {}: {error}",
            temporary.display(),
            path.display()
        )
    })
}

fn env_threshold(name: &str, default: f32) -> f32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(default)
}

fn has_profiles(directory: &Path) -> bool {
    fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
        })
}

fn profiles_dir() -> PathBuf {
    std::env::var("FRIDAY_IDENTITIES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("identities"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(embedding: Vec<f32>) -> IdentityProfile {
        IdentityProfile {
            version: PROFILE_VERSION,
            person_id: "admin".to_string(),
            display_name: "Kaz".to_string(),
            model: "sface.onnx".to_string(),
            model_sha256: "digest".to_string(),
            created_at_unix: 1,
            embedding_count: 1,
            embeddings: vec![embedding],
        }
    }

    #[test]
    fn confirms_one_strong_face_match() {
        let result = best_profile_match(&[vec![1.0, 0.0]], &[profile(vec![1.0, 0.0])], 0.50, 0.65);
        assert!(result.is_known());
        assert_eq!(result.display_name.as_deref(), Some("Kaz"));
    }

    #[test]
    fn leaves_a_weak_face_unknown() {
        let result = best_profile_match(&[vec![0.0, 1.0]], &[profile(vec![1.0, 0.0])], 0.50, 0.65);
        assert_eq!(result.status, "unknown");
        assert!(!result.is_known());
    }
}
