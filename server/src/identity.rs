//! Local known-person recognition for stored FRIDAY events.
//!
//! This is deliberately separate from Ollama: a small Python sidecar uses
//! OpenCV YuNet + SFace against local, gitignored embedding profiles. Source
//! images are not sent anywhere, and recognition failure never affects event
//! storage, AI analysis, or notification of an unknown person.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::ai::Identity;

const DEFAULT_MATCH_THRESHOLD: &str = "0.50";
const DEFAULT_STRONG_MATCH_THRESHOLD: &str = "0.65";

pub fn recognize_event(thumbnail_path: &Path, keyframe_paths: &[PathBuf]) -> Identity {
    let profiles_dir = profiles_dir();
    if !has_profiles(&profiles_dir) {
        return Identity::not_enrolled();
    }

    let script = identity_script_path();
    if !script.is_file() {
        println!("identity: sidecar not found at {}", script.display());
        return Identity::failed();
    }

    let mut images = Vec::with_capacity(keyframe_paths.len() + 1);
    images.push(thumbnail_path);
    images.extend(keyframe_paths.iter().map(PathBuf::as_path));

    let threshold = std::env::var("FRIDAY_FACE_MATCH_THRESHOLD")
        .unwrap_or_else(|_| DEFAULT_MATCH_THRESHOLD.to_string());
    let strong_threshold = std::env::var("FRIDAY_FACE_STRONG_MATCH_THRESHOLD")
        .unwrap_or_else(|_| DEFAULT_STRONG_MATCH_THRESHOLD.to_string());
    let output = Command::new(identity_python())
        .arg(script)
        .arg("recognize")
        .arg("--profiles-dir")
        .arg(&profiles_dir)
        .arg("--threshold")
        .arg(threshold)
        .arg("--strong-threshold")
        .arg(strong_threshold)
        .args(images)
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                println!("identity: sidecar notes: {}", stderr.trim());
            }
            match parse_identity_output(&stdout) {
                Some(identity) => {
                    match identity.display_name.as_deref() {
                        Some(name) if identity.is_known() => println!(
                            "identity: recognized {name} ({:.1}% cosine confidence)",
                            identity.confidence.unwrap_or_default() * 100.0
                        ),
                        _ => println!("identity: {}", identity.status),
                    }
                    identity
                }
                None => {
                    println!("identity: invalid sidecar response: {}", stdout.trim());
                    Identity::failed()
                }
            }
        }
        Ok(output) => {
            println!(
                "identity: sidecar failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Identity::failed()
        }
        Err(error) => {
            println!("identity: failed to start sidecar: {error}");
            Identity::failed()
        }
    }
}

fn parse_identity_output(output: &str) -> Option<Identity> {
    let identity: Identity = serde_json::from_str(output.trim()).ok()?;
    identity.is_valid().then_some(identity)
}

fn has_profiles(directory: &Path) -> bool {
    std::fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
}

fn profiles_dir() -> PathBuf {
    std::env::var("FRIDAY_IDENTITIES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("identities"))
}

fn identity_script_path() -> PathBuf {
    if let Ok(path) = std::env::var("FRIDAY_IDENTITY_SCRIPT") {
        return PathBuf::from(path);
    }
    repo_root().join("scripts").join("face_identity.py")
}

fn identity_python() -> String {
    if let Ok(path) = std::env::var("FRIDAY_TRACKER_PYTHON") {
        return path;
    }
    let venv_python = repo_root().join(".venv-tracker").join("bin").join("python");
    if venv_python.is_file() {
        venv_python.to_string_lossy().into_owned()
    } else {
        "python3".to_string()
    }
}

fn repo_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap_or(manifest).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_valid_known_identity() {
        let identity = parse_identity_output(
            r#"{"status":"known","known_person_id":"admin","display_name":"Admin","confidence":0.82}"#,
        )
        .unwrap();
        assert!(identity.is_known());
        assert_eq!(identity.display_name.as_deref(), Some("Admin"));
    }

    #[test]
    fn rejects_inconsistent_known_identity() {
        assert!(parse_identity_output(
            r#"{"status":"known","known_person_id":null,"display_name":"Admin","confidence":0.82}"#
        )
        .is_none());
    }

    #[test]
    fn accepts_a_no_face_result() {
        let identity = parse_identity_output(
            r#"{"status":"no_face","known_person_id":null,"display_name":null,"confidence":null}"#,
        )
        .unwrap();
        assert!(!identity.is_known());
    }

    #[test]
    fn accepts_multiple_faces_without_suppressing_as_known() {
        let identity = parse_identity_output(
            r#"{"status":"multiple_faces","known_person_id":null,"display_name":null,"confidence":null}"#,
        )
        .unwrap();
        assert!(!identity.is_known());
    }
}
