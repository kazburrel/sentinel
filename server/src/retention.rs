//! Server-side retention cleanup: deletes stored event file sets once
//! they're older than `EVENT_RETENTION_DAYS` (default 30), so
//! `server/uploads/` doesn't grow forever. Runs on its own background
//! thread, independent of request handling -- the same reasoning as the
//! `ai` module's detached analysis thread: this server's accept loop is
//! single-threaded and synchronous, so a sweep must never run in-line with
//! (or block) an upload.
//!
//! Deliberately conservative about what it touches: only files matching
//! this server's own `event_<timestamp>_<label>...` naming scheme
//! (`commit_part_file` in `main.rs`, and `ai::analyze_and_save`) are ever
//! considered, and a whole event set (thumbnail, video, `analysis.json`,
//! whatever's present) is deleted together, only once *every* file in it
//! has aged past the retention window -- never partially, and never a set
//! that's still gaining new files (e.g. `analysis.json` landing a few
//! seconds after the thumbnail, or a video part not committed yet). The
//! ESP32 SD-card offline queue (`firmware::queue`) is entirely separate
//! storage on the device itself and is never touched here.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const DEFAULT_RETENTION_DAYS: u64 = 30;
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// Reads `EVENT_RETENTION_DAYS`, defaulting to `DEFAULT_RETENTION_DAYS`
/// when unset or unparseable.
pub fn retention_from_env() -> Duration {
    let days = std::env::var("EVENT_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS);
    Duration::from_secs(days * SECONDS_PER_DAY)
}

/// Extracts the `event_<timestamp>` grouping key from a stored file name
/// (e.g. `event_1783612725863_thumbnail.jpg` -> `event_1783612725863`,
/// and equally `event_1783612725863_thumbnail_1.jpg` -- the hard-link
/// collision suffix `commit_part_file` sometimes adds -- groups under the
/// same key). Returns `None` for anything that doesn't match this scheme
/// at all: temp files (`.tmp_...`), unrelated files a user might have
/// dropped in the directory, or anything else -- all left completely
/// alone by this module.
fn event_key(file_name: &str) -> Option<String> {
    let rest = file_name.strip_prefix("event_")?;
    let (timestamp, _) = rest.split_once('_')?;
    if !timestamp.is_empty() && timestamp.bytes().all(|b| b.is_ascii_digit()) {
        Some(format!("event_{timestamp}"))
    } else {
        None
    }
}

/// Scans `dir` for complete event sets whose *most recently modified*
/// file is already older than `retention`, deletes every file in each such
/// set, and returns how many sets were removed. Using the most-recently
/// modified file (rather than the oldest) as the deciding age is what
/// guarantees a set is never deleted while any part of it is still being
/// written or has just landed -- see the module doc.
pub fn clean_expired_events(dir: &Path, retention: Duration, now: SystemTime) -> std::io::Result<usize> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };

    let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if let Some(key) = event_key(file_name) {
            groups.entry(key).or_default().push(entry.path());
        }
    }

    let mut deleted_sets = 0;
    for (key, paths) in groups {
        match newest_age_in_set(&paths, now) {
            Some(newest_age) if newest_age >= retention => {
                for path in &paths {
                    match fs::remove_file(path) {
                        Ok(()) => println!("retention: deleted {}", path.display()),
                        Err(e) => println!("retention: failed to delete {}: {e}", path.display()),
                    }
                }
                println!("retention: removed expired event set {key} ({} file(s))", paths.len());
                deleted_sets += 1;
            }
            // Either not old enough yet, or metadata couldn't be read
            // (e.g. a file vanished mid-scan) / a file's mtime is in the
            // future (clock skew) -- either way, safest is to leave the
            // whole set alone and reconsider on the next sweep.
            _ => {}
        }
    }

    Ok(deleted_sets)
}

/// The age (time since last modified) of the *most recently* modified
/// file in `paths` -- `None` if any file's metadata/mtime couldn't be read
/// at all, which the caller treats as "don't touch this set".
fn newest_age_in_set(paths: &[PathBuf], now: SystemTime) -> Option<Duration> {
    let mut min_age: Option<Duration> = None;
    for path in paths {
        let modified = fs::metadata(path).ok()?.modified().ok()?;
        let age = now.duration_since(modified).ok()?;
        min_age = Some(match min_age {
            Some(existing) => existing.min(age),
            None => age,
        });
    }
    min_age
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File, FileTimes};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let dir = std::env::temp_dir().join(format!("camera_server_test_retention_{label}_{unique}"));
        fs::create_dir_all(&dir).expect("scratch dir should be creatable");
        dir
    }

    /// Creates `name` under `dir` with content `b"x"` and backdates its
    /// mtime by `age` from `now` -- the whole point of these tests is
    /// exercising real age comparisons, not just presence/absence.
    fn touch_aged(dir: &Path, name: &str, age: Duration, now: SystemTime) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, b"x").expect("scratch file should be writable");
        let file = File::options().write(true).open(&path).expect("reopen for set_times");
        file.set_times(FileTimes::new().set_modified(now - age)).expect("set_times should succeed");
        path
    }

    const THIRTY_DAYS: Duration = Duration::from_secs(30 * 24 * 60 * 60);

    #[test]
    fn deletes_a_complete_event_set_past_retention() {
        let dir = scratch_dir("expired_set");
        let now = SystemTime::now();
        let old = THIRTY_DAYS + Duration::from_secs(3600);
        touch_aged(&dir, "event_100_thumbnail.jpg", old, now);
        touch_aged(&dir, "event_100_video.bin", old, now);
        touch_aged(&dir, "event_100_analysis.json", old, now);

        let deleted = clean_expired_events(&dir, THIRTY_DAYS, now).expect("sweep should succeed");
        assert_eq!(deleted, 1);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "every file in the set should be gone");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keeps_a_set_that_has_not_aged_out_yet() {
        let dir = scratch_dir("fresh_set");
        let now = SystemTime::now();
        touch_aged(&dir, "event_200_thumbnail.jpg", Duration::from_secs(60), now);
        touch_aged(&dir, "event_200_video.bin", Duration::from_secs(60), now);

        let deleted = clean_expired_events(&dir, THIRTY_DAYS, now).expect("sweep should succeed");
        assert_eq!(deleted, 0);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2, "nothing should be removed");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn never_partially_deletes_a_set_still_gaining_files() {
        // Simulates analysis.json landing well after the thumbnail/video
        // (the AI analysis background thread can take several seconds to
        // tens of seconds) -- even if the thumbnail/video are expired, the
        // whole set must be left alone while any member is still fresh.
        let dir = scratch_dir("partial_set");
        let now = SystemTime::now();
        let old = THIRTY_DAYS + Duration::from_secs(3600);
        touch_aged(&dir, "event_300_thumbnail.jpg", old, now);
        touch_aged(&dir, "event_300_video.bin", old, now);
        touch_aged(&dir, "event_300_analysis.json", Duration::from_secs(5), now);

        let deleted = clean_expired_events(&dir, THIRTY_DAYS, now).expect("sweep should succeed");
        assert_eq!(deleted, 0);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 3, "no file in the set should be removed yet");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn never_deletes_files_outside_its_own_naming_scheme() {
        let dir = scratch_dir("unrelated_files");
        let now = SystemTime::now();
        let very_old = THIRTY_DAYS * 10;
        // A user-dropped file, a stray temp file from an interrupted
        // commit, and a differently-shaped name that merely starts with
        // "event_" -- none of these match `event_<digits>_...` and none
        // should ever be touched, no matter how old.
        touch_aged(&dir, "README.txt", very_old, now);
        touch_aged(&dir, ".tmp_100_thumbnail_0", very_old, now);
        touch_aged(&dir, "event_notes.md", very_old, now);
        touch_aged(&dir, "eventually_something.jpg", very_old, now);

        let deleted = clean_expired_events(&dir, THIRTY_DAYS, now).expect("sweep should succeed");
        assert_eq!(deleted, 0);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 4, "unrelated files must never be deleted");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn groups_a_hard_link_collision_suffix_into_the_same_set() {
        let dir = scratch_dir("collision_suffix");
        let now = SystemTime::now();
        let old = THIRTY_DAYS + Duration::from_secs(3600);
        touch_aged(&dir, "event_400_thumbnail.jpg", old, now);
        // commit_part_file's own collision-suffix naming (see main.rs).
        touch_aged(&dir, "event_400_thumbnail_1.jpg", old, now);

        let deleted = clean_expired_events(&dir, THIRTY_DAYS, now).expect("sweep should succeed");
        assert_eq!(deleted, 1, "both files share event_400 and should be treated as one set");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn different_events_are_independent() {
        let dir = scratch_dir("independent_events");
        let now = SystemTime::now();
        let old = THIRTY_DAYS + Duration::from_secs(3600);
        touch_aged(&dir, "event_500_thumbnail.jpg", old, now);
        touch_aged(&dir, "event_600_thumbnail.jpg", Duration::from_secs(60), now);

        let deleted = clean_expired_events(&dir, THIRTY_DAYS, now).expect("sweep should succeed");
        assert_eq!(deleted, 1);
        let remaining: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(remaining.len(), 1, "only the expired event's file should be gone");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_directory_is_not_an_error() {
        let dir = std::env::temp_dir().join("camera_server_test_retention_never_created");
        let deleted = clean_expired_events(&dir, THIRTY_DAYS, SystemTime::now()).expect("missing dir is fine, not an error");
        assert_eq!(deleted, 0);
    }

    #[test]
    fn event_key_rejects_non_matching_names() {
        assert_eq!(event_key("event_123_thumbnail.jpg"), Some("event_123".to_string()));
        assert_eq!(event_key("event_123_thumbnail_1.jpg"), Some("event_123".to_string()));
        assert_eq!(event_key(".tmp_123_thumbnail_0"), None);
        assert_eq!(event_key("event_notes.md"), None);
        assert_eq!(event_key("README.txt"), None);
        assert_eq!(event_key("event_"), None);
    }
}
