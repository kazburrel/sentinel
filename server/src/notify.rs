//! Server-side notifications for actionable door-camera events.
//!
//! This deliberately runs after local storage and AI analysis: a notification
//! is a side effect, never part of whether the ESP32 upload succeeded. The
//! first real sink is Telegram, configured only by environment variables so
//! secrets do not enter git:
//!
//! - `TELEGRAM_BOT_TOKEN`
//! - `TELEGRAM_CHAT_ID`
//!
//! If either is missing, the server falls back to a console notifier that
//! prints what it would have sent. Notification policy is intentionally
//! conservative: alert for actionable events (person, package, or high
//! importance) and keep low/empty motion local-only. Vehicle and animal
//! notifications are opt-in because they are noisy for many door-camera
//! placements.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ureq::unversioned::multipart::Form;

use crate::ai::{AnalysisRegion, EventAnalysis, Importance, RegionKind};
use crate::video;

const TELEGRAM_TIMEOUT: Duration = Duration::from_secs(10);
const TELEGRAM_POLL_TIMEOUT_SECS: u64 = 20;
const TELEGRAM_PHOTO_CAPTION_SOFT_LIMIT: usize = 900;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertCategory {
    Visitor,
    Package,
    SecurityConcern,
    HighImportance,
    Vehicle,
    Animal,
    OtherMotion,
}

impl AlertCategory {
    fn label(self) -> &'static str {
        match self {
            AlertCategory::Visitor => "visitor/person",
            AlertCategory::Package => "delivery/package",
            AlertCategory::SecurityConcern => "security concern",
            AlertCategory::HighImportance => "high importance",
            AlertCategory::Vehicle => "vehicle",
            AlertCategory::Animal => "animal",
            AlertCategory::OtherMotion => "other motion",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Alert {
    pub categories: Vec<AlertCategory>,
    pub analysis: EventAnalysis,
    pub thumbnail_path: PathBuf,
}

impl Alert {
    pub fn event_id(&self) -> Option<String> {
        event_id_from_path(&self.thumbnail_path)
    }

    pub fn message(&self) -> String {
        let categories = self
            .categories
            .iter()
            .map(|c| c.label())
            .collect::<Vec<_>>()
            .join(", ");
        let event = self.event_id().unwrap_or_else(|| "unknown".to_string());
        let summary = if self.analysis.plain_description.trim().is_empty() {
            self.analysis.description.as_str()
        } else {
            self.analysis.plain_description.as_str()
        };
        let mut message = format!(
            "🚪 Project FRIDAY\nEvent #{event}\n\n{}\n\nPriority: {}\nConfidence: {}%\nDetected: {categories}",
            summary,
            priority_label(self.analysis.importance),
            self.analysis.confidence.min(100),
        );

        if !self.analysis.notable_actions.is_empty() {
            message.push_str("\n\nNotable:");
            for action in self.analysis.notable_actions.iter().take(4) {
                message.push_str("\n• ");
                message.push_str(action);
            }
        }

        if !self.analysis.concerning_details.is_empty() {
            message.push_str("\n\nConcern:");
            for detail in self.analysis.concerning_details.iter().take(4) {
                message.push_str("\n• ");
                message.push_str(detail);
            }
        }

        if !self.analysis.likely_intent.trim().is_empty() {
            message.push_str("\n\nLikely intent: ");
            message.push_str(&self.analysis.likely_intent);
        }

        if !self.analysis.recommended_action.trim().is_empty() {
            message.push_str("\nRecommended action: ");
            message.push_str(&self.analysis.recommended_action);
        }

        if let Some(comment) = friday_comment_for(&self.analysis) {
            message.push_str("\n\nFRIDAY says: ");
            message.push_str(comment);
        }

        if !self.analysis.reason.is_empty() {
            message.push_str("\n\nReason:");
            for reason in self.analysis.reason.iter().take(4) {
                message.push_str("\n• ");
                message.push_str(reason);
            }
        }

        if !self.analysis.timeline.is_empty() {
            message.push_str("\n\nTimeline:");
            for item in self.analysis.timeline.iter().take(5) {
                message.push_str("\n• ");
                message.push_str(item);
            }
        }

        message.push_str("\n\nTap Send video below, or reply: video latest, events, or help");
        message
    }
}

fn priority_label(importance: Importance) -> &'static str {
    match importance {
        Importance::Low => "Low",
        Importance::Medium => "Medium",
        Importance::High => "High",
        Importance::Critical => "Critical",
    }
}

fn friday_comment_for(analysis: &EventAnalysis) -> Option<&str> {
    let model_comment = analysis.friday_comment.trim();
    if !model_comment.is_empty() {
        return Some(model_comment);
    }

    if analysis.concerning_object || matches!(analysis.importance, Importance::Critical) {
        Some("That prop choice is not helping their case.")
    } else if analysis.concerning_behavior {
        Some("Bold choice, considering the camera keeps receipts.")
    } else {
        None
    }
}

fn photo_caption_for_alert(alert: &Alert, full_message: &str) -> String {
    if full_message.chars().count() <= TELEGRAM_PHOTO_CAPTION_SOFT_LIMIT {
        return full_message.to_string();
    }

    let categories = alert
        .categories
        .iter()
        .map(|c| c.label())
        .collect::<Vec<_>>()
        .join(", ");
    let event = alert.event_id().unwrap_or_else(|| "unknown".to_string());
    let summary = if alert.analysis.plain_description.trim().is_empty() {
        alert.analysis.description.as_str()
    } else {
        alert.analysis.plain_description.as_str()
    };

    let mut caption = format!(
        "🚪 Project FRIDAY\nEvent #{event}\n\n{}\n\nPriority: {}\nConfidence: {}%\nDetected: {categories}",
        trim_for_caption(summary, 260),
        priority_label(alert.analysis.importance),
        alert.analysis.confidence.min(100),
    );

    if let Some(comment) = friday_comment_for(&alert.analysis) {
        caption.push_str("\n\nFRIDAY says: ");
        caption.push_str(&trim_for_caption(comment, 120));
    }

    caption.push_str(
        "\n\nFull details sent below.\nTap Send video below, or reply: video latest, events, or help",
    );

    if caption.chars().count() > TELEGRAM_PHOTO_CAPTION_SOFT_LIMIT {
        format!(
            "🚪 Project FRIDAY\nEvent #{event}\n\n{}\n\nPriority: {}\nConfidence: {}%\n\nFull details sent below.\nTap Send video below, or reply: video latest",
            trim_for_caption(summary, 180),
            priority_label(alert.analysis.importance),
            alert.analysis.confidence.min(100),
        )
    } else {
        caption
    }
}

fn trim_for_caption(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let mut out = trimmed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

#[derive(Debug, Clone)]
pub struct NotificationPolicy {
    notify_person: bool,
    notify_package: bool,
    notify_high: bool,
    notify_vehicle: bool,
    notify_animal: bool,
    notify_low: bool,
}

impl Default for NotificationPolicy {
    fn default() -> Self {
        Self {
            notify_person: true,
            notify_package: true,
            notify_high: true,
            notify_vehicle: false,
            notify_animal: false,
            notify_low: false,
        }
    }
}

impl NotificationPolicy {
    pub fn from_env() -> Self {
        let mut policy = Self::default();
        policy.notify_person = env_bool("NOTIFY_PERSON").unwrap_or(policy.notify_person);
        policy.notify_package = env_bool("NOTIFY_PACKAGE").unwrap_or(policy.notify_package);
        policy.notify_high = env_bool("NOTIFY_HIGH").unwrap_or(policy.notify_high);
        policy.notify_vehicle = env_bool("NOTIFY_VEHICLES").unwrap_or(policy.notify_vehicle);
        policy.notify_animal = env_bool("NOTIFY_ANIMALS").unwrap_or(policy.notify_animal);
        policy.notify_low = env_bool("NOTIFY_LOW").unwrap_or(policy.notify_low);
        policy
    }

    pub fn alert_for(&self, analysis: &EventAnalysis, thumbnail_path: &Path, _analysis_path: &Path) -> Option<Alert> {
        let mut categories = Vec::new();

        if self.notify_person && analysis.person {
            categories.push(AlertCategory::Visitor);
        }
        if self.notify_package && analysis.package {
            categories.push(AlertCategory::Package);
        }
        if analysis.concerning_object || analysis.concerning_behavior {
            categories.push(AlertCategory::SecurityConcern);
        }
        if self.notify_high && matches!(analysis.importance, Importance::High | Importance::Critical) {
            categories.push(AlertCategory::HighImportance);
        }
        if self.notify_vehicle && analysis.vehicle {
            categories.push(AlertCategory::Vehicle);
        }
        if self.notify_animal && analysis.animal {
            categories.push(AlertCategory::Animal);
        }
        if self.notify_low && analysis.importance == Importance::Low && categories.is_empty() {
            categories.push(AlertCategory::OtherMotion);
        }

        (!categories.is_empty()).then(|| Alert {
            categories,
            analysis: analysis.clone(),
            thumbnail_path: thumbnail_path.to_path_buf(),
        })
    }
}

fn env_bool(name: &str) -> Option<bool> {
    let value = std::env::var(name).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[derive(Debug)]
pub enum NotifyError {
    /// Read via this enum's `Debug` impl in notification failure logs; the
    /// dead-code lint does not count that as reading the field.
    #[allow(dead_code)]
    Request(String),
}

pub trait Notifier: Send + Sync {
    fn notify(&self, alert: &Alert) -> Result<(), NotifyError>;
    fn name(&self) -> &str;
}

pub struct ConsoleNotifier;

impl Notifier for ConsoleNotifier {
    fn notify(&self, alert: &Alert) -> Result<(), NotifyError> {
        println!("notification(console):\n{}", alert.message());
        Ok(())
    }

    fn name(&self) -> &str {
        "console"
    }
}

pub struct TelegramNotifier {
    bot_token: String,
    chat_id: String,
    timeout: Duration,
}

impl TelegramNotifier {
    pub fn new(bot_token: impl Into<String>, chat_id: impl Into<String>, timeout: Duration) -> Self {
        Self {
            bot_token: bot_token.into(),
            chat_id: chat_id.into(),
            timeout,
        }
    }

    fn send_message(&self, text: &str) -> Result<(), NotifyError> {
        #[derive(Serialize)]
        struct SendMessage<'a> {
            chat_id: &'a str,
            text: &'a str,
        }

        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
        ureq::post(url.as_str())
            .config()
            .timeout_global(Some(self.timeout))
            .build()
            .send_json(SendMessage {
                chat_id: &self.chat_id,
                text,
            })
            .map(|_| ())
            .map_err(|e| NotifyError::Request(e.to_string()))
    }

    fn answer_callback_query(&self, callback_query_id: &str, text: &str) -> Result<(), NotifyError> {
        #[derive(Serialize)]
        struct AnswerCallbackQuery<'a> {
            callback_query_id: &'a str,
            text: &'a str,
        }

        let url = format!("https://api.telegram.org/bot{}/answerCallbackQuery", self.bot_token);
        ureq::post(url.as_str())
            .config()
            .timeout_global(Some(self.timeout))
            .build()
            .send_json(AnswerCallbackQuery {
                callback_query_id,
                text,
            })
            .map(|_| ())
            .map_err(|e| NotifyError::Request(e.to_string()))
    }

    fn send_document(&self, path: &Path, caption: &str) -> Result<(), NotifyError> {
        let url = format!("https://api.telegram.org/bot{}/sendDocument", self.bot_token);
        let form = Form::new()
            .text("chat_id", &self.chat_id)
            .text("caption", caption)
            .file("document", path)
            .map_err(|e| NotifyError::Request(e.to_string()))?;

        ureq::post(url.as_str())
            .config()
            .timeout_global(Some(self.timeout))
            .build()
            .send(form)
            .map(|_| ())
            .map_err(|e| NotifyError::Request(e.to_string()))
    }

    fn send_photo(&self, path: &Path, caption: &str) -> Result<(), NotifyError> {
        let url = format!("https://api.telegram.org/bot{}/sendPhoto", self.bot_token);
        let form = Form::new()
            .text("chat_id", &self.chat_id)
            .text("caption", caption)
            .file("photo", path)
            .map_err(|e| NotifyError::Request(e.to_string()))?;

        ureq::post(url.as_str())
            .config()
            .timeout_global(Some(self.timeout))
            .build()
            .send(form)
            .map(|_| ())
            .map_err(|e| NotifyError::Request(e.to_string()))
    }

    fn get_updates(&self, offset: Option<i64>) -> Result<Vec<TelegramUpdate>, NotifyError> {
        let mut url = format!(
            "https://api.telegram.org/bot{}/getUpdates?timeout={TELEGRAM_POLL_TIMEOUT_SECS}",
            self.bot_token
        );
        if let Some(offset) = offset {
            url.push_str("&offset=");
            url.push_str(&offset.to_string());
        }

        let mut response = ureq::get(url.as_str())
            .config()
            .timeout_global(Some(Duration::from_secs(TELEGRAM_POLL_TIMEOUT_SECS + 5)))
            .build()
            .call()
            .map_err(|e| NotifyError::Request(e.to_string()))?;

        let body: TelegramUpdatesResponse = response
            .body_mut()
            .read_json()
            .map_err(|e| NotifyError::Request(e.to_string()))?;
        Ok(body.result)
    }
}

impl Notifier for TelegramNotifier {
    fn notify(&self, alert: &Alert) -> Result<(), NotifyError> {
        let full_message = alert.message();
        let caption = photo_caption_for_alert(alert, &full_message);
        let url = format!("https://api.telegram.org/bot{}/sendPhoto", self.bot_token);
        let reply_markup = alert.event_id().map(send_video_reply_markup);

        let mut form = Form::new()
            .text("chat_id", &self.chat_id)
            .text("caption", &caption)
            .file("photo", &alert.thumbnail_path)
            .map_err(|e| NotifyError::Request(e.to_string()))?;
        if let Some(markup) = &reply_markup {
            form = form.text("reply_markup", markup);
        }

        match ureq::post(url.as_str())
            .config()
            .timeout_global(Some(self.timeout))
            .build()
            .send(form)
        {
            Ok(_) => {
                if caption != full_message {
                    self.send_message(&full_message)?;
                }
                Ok(())
            }
            Err(photo_err) => {
                println!("notification(telegram): sendPhoto failed, falling back to text: {photo_err}");
                self.send_message(&full_message)
            }
        }
    }

    fn name(&self) -> &str {
        "telegram"
    }
}

pub fn notifier_from_env() -> Box<dyn Notifier> {
    match (std::env::var("TELEGRAM_BOT_TOKEN"), std::env::var("TELEGRAM_CHAT_ID")) {
        (Ok(token), Ok(chat_id)) if !token.trim().is_empty() && !chat_id.trim().is_empty() => {
            Box::new(TelegramNotifier::new(token, chat_id, TELEGRAM_TIMEOUT))
        }
        _ => Box::new(ConsoleNotifier),
    }
}

#[derive(Debug, Deserialize)]
struct TelegramUpdatesResponse {
    result: Vec<TelegramUpdate>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
    callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    text: Option<String>,
    chat: TelegramChat,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramCallbackQuery {
    id: String,
    data: Option<String>,
    message: Option<TelegramCallbackMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramCallbackMessage {
    chat: TelegramChat,
}

fn event_id_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("event_")?;
    let id = rest.split('_').next()?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VideoCommand {
    event_id: String,
    force_resend: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TelegramCommand {
    Help,
    Events,
    Summary(String),
    Photo(String),
    Analysis(String),
    Video(VideoCommand),
}

fn parse_telegram_command(text: &str) -> Option<TelegramCommand> {
    let trimmed = text.trim();
    let normalized = trimmed.trim_start_matches('/').trim();
    if normalized.eq_ignore_ascii_case("help")
        || normalized.eq_ignore_ascii_case("start")
        || normalized.eq_ignore_ascii_case("commands")
    {
        return Some(TelegramCommand::Help);
    }
    if normalized.eq_ignore_ascii_case("events") {
        return Some(TelegramCommand::Events);
    }
    if normalized.eq_ignore_ascii_case("resend latest") || normalized.eq_ignore_ascii_case("resend last") {
        return Some(TelegramCommand::Video(VideoCommand {
            event_id: "latest".to_string(),
            force_resend: true,
        }));
    }
    parse_event_lookup_command(normalized).or_else(|| parse_video_command(normalized).map(TelegramCommand::Video))
}

fn parse_event_lookup_command(text: &str) -> Option<TelegramCommand> {
    let mut parts = text.split_whitespace();
    let command = parts.next()?;
    let event_id = parts.next().unwrap_or("latest");
    if parts.next().is_some() {
        return None;
    }
    let event_id = normalize_event_id_arg(event_id)?;

    if command.eq_ignore_ascii_case("summary") {
        Some(TelegramCommand::Summary(event_id))
    } else if command.eq_ignore_ascii_case("photo") || command.eq_ignore_ascii_case("snapshot") {
        Some(TelegramCommand::Photo(event_id))
    } else if command.eq_ignore_ascii_case("analysis") {
        Some(TelegramCommand::Analysis(event_id))
    } else {
        None
    }
}

fn normalize_event_id_arg(event_id: &str) -> Option<String> {
    let event_id = event_id.trim();
    if event_id.eq_ignore_ascii_case("latest") || event_id.eq_ignore_ascii_case("last") {
        return Some("latest".to_string());
    }
    (!event_id.is_empty() && event_id.bytes().all(|b| b.is_ascii_digit())).then(|| event_id.to_string())
}

fn parse_video_command(text: &str) -> Option<VideoCommand> {
    let mut parts = text.split_whitespace();
    let command = parts.next()?;
    if !command.eq_ignore_ascii_case("video") {
        return None;
    }
    let event_id = parts.next()?;
    let force_resend = match parts.next() {
        Some(word) if word.eq_ignore_ascii_case("again") || word.eq_ignore_ascii_case("resend") => true,
        Some(_) => return None,
        None => false,
    };
    if parts.next().is_some() {
        return None;
    }
    let event_id = normalize_event_id_arg(event_id)?;
    Some(VideoCommand {
        event_id: event_id.to_string(),
        force_resend,
    })
}

fn parse_video_callback(data: &str) -> Option<String> {
    let event_id = data.strip_prefix("video:")?;
    (!event_id.is_empty() && event_id.bytes().all(|b| b.is_ascii_digit())).then(|| event_id.to_string())
}

fn send_video_reply_markup(event_id: String) -> String {
    serde_json::json!({
        "inline_keyboard": [[
            {
                "text": "Send video",
                "callback_data": format!("video:{event_id}")
            }
        ]]
    })
    .to_string()
}

fn playable_video_path_for_event(uploads_dir: &Path, event_id: &str) -> Option<PathBuf> {
    let path = if event_id.eq_ignore_ascii_case("latest") || event_id.eq_ignore_ascii_case("last") {
        latest_playable_video_path(uploads_dir)?
    } else {
        if event_id.is_empty() || !event_id.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mp4 = uploads_dir.join(format!("event_{event_id}_video.mp4"));
        if mp4.is_file() {
            mp4
        } else {
            let raw = uploads_dir.join(format!("event_{event_id}_video.bin"));
            raw.is_file().then(|| video::convert_to_mp4(&raw)).flatten()?
        }
    };
    Some(annotate_video_if_possible(uploads_dir, &path))
}

fn event_id_for_raw_video_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("event_")?;
    let id = rest.strip_suffix("_video.bin")?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_string())
}

fn event_id_for_playable_video_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("event_")?;
    let id = rest
        .strip_suffix("_video.mp4")
        .or_else(|| rest.strip_suffix("_annotated.mp4"))
        .or_else(|| rest.strip_suffix("_locked.mp4"))
        .or_else(|| rest.strip_suffix("_locked_normal.mp4"))
        .or_else(|| rest.strip_suffix("_locked_minimal.mp4"))
        .or_else(|| rest.strip_suffix("_locked_threat.mp4"))
        .or_else(|| rest.strip_suffix("_locked_frame_normal.mp4"))
        .or_else(|| rest.strip_suffix("_locked_frame_minimal.mp4"))
        .or_else(|| rest.strip_suffix("_locked_frame_threat.mp4"))
        .or_else(|| rest.strip_suffix("_locked_latched_normal.mp4"))
        .or_else(|| rest.strip_suffix("_locked_latched_minimal.mp4"))
        .or_else(|| rest.strip_suffix("_locked_latched_threat.mp4"))
        .or_else(|| rest.strip_suffix("_locked_stable_normal.mp4"))
        .or_else(|| rest.strip_suffix("_locked_stable_minimal.mp4"))
        .or_else(|| rest.strip_suffix("_locked_stable_threat.mp4"))
        .or_else(|| rest.strip_suffix("_locked_reactive_normal.mp4"))
        .or_else(|| rest.strip_suffix("_locked_reactive_minimal.mp4"))
        .or_else(|| rest.strip_suffix("_locked_reactive_threat.mp4"))?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_string())
}

fn latest_raw_video_path(uploads_dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(uploads_dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if event_id_for_raw_video_path(&path).is_none() {
            continue;
        }
        let modified = entry.metadata().ok()?.modified().ok()?;
        if best.as_ref().is_none_or(|(best_modified, _)| modified > *best_modified) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

fn latest_playable_video_path(uploads_dir: &Path) -> Option<PathBuf> {
    let raw = latest_raw_video_path(uploads_dir)?;
    let event_id = event_id_for_raw_video_path(&raw)?;
    let mp4 = uploads_dir.join(format!("event_{event_id}_video.mp4"));
    if mp4.is_file() {
        Some(mp4)
    } else {
        video::convert_to_mp4(&raw)
    }
}

fn annotate_video_if_possible(uploads_dir: &Path, video_path: &Path) -> PathBuf {
    let Some(event_id) = event_id_for_playable_video_path(video_path) else {
        return video_path.to_path_buf();
    };
    let Some((_, analysis)) = analysis_for_event(uploads_dir, &event_id) else {
        return video_path.to_path_buf();
    };
    if let Some(locked_path) =
        video::lock_mp4_with_tracker(video_path, tracker_threat_level(&analysis), analysis.concerning_behavior)
    {
        return locked_path;
    }
    let labels = overlay_labels_for_analysis(&analysis);
    let regions = overlay_regions_for_analysis(&analysis);
    video::annotate_mp4(video_path, &labels, &regions).unwrap_or_else(|| video_path.to_path_buf())
}

fn tracker_threat_level(analysis: &EventAnalysis) -> video::TrackerThreatLevel {
    if analysis.concerning_object || matches!(analysis.importance, Importance::Critical) {
        return video::TrackerThreatLevel::Threat;
    }
    if analysis.concerning_behavior || matches!(analysis.importance, Importance::High) {
        return video::TrackerThreatLevel::Minimal;
    }
    video::TrackerThreatLevel::Normal
}

fn overlay_labels_for_analysis(analysis: &EventAnalysis) -> Vec<video::OverlayLabel> {
    let mut labels = Vec::new();
    if analysis.concerning_object {
        labels.push(video::OverlayLabel::new("CONCERNING OBJECT", video::OverlayColor::Red));
    }
    if analysis.concerning_behavior {
        labels.push(video::OverlayLabel::new("CONCERNING BEHAVIOR", video::OverlayColor::Red));
    }
    if analysis.person {
        let color = if analysis.concerning_object || analysis.concerning_behavior {
            video::OverlayColor::Red
        } else if matches!(analysis.importance, Importance::High | Importance::Critical) {
            video::OverlayColor::Yellow
        } else {
            video::OverlayColor::White
        };
        labels.push(video::OverlayLabel::new("PERSON / FACE REGION", color));
    }
    if analysis.package {
        labels.push(video::OverlayLabel::new("PACKAGE", video::OverlayColor::Green));
    }
    if analysis.vehicle {
        labels.push(video::OverlayLabel::new("VEHICLE", video::OverlayColor::White));
    }
    if analysis.animal {
        labels.push(video::OverlayLabel::new("ANIMAL", video::OverlayColor::White));
    }
    for action in analysis.notable_actions.iter().take(2) {
        labels.push(video::OverlayLabel::new(action.to_ascii_uppercase(), video::OverlayColor::Yellow));
    }
    for detail in analysis.concerning_details.iter().take(2) {
        labels.push(video::OverlayLabel::new(detail.to_ascii_uppercase(), video::OverlayColor::Red));
    }
    if labels.is_empty() {
        labels.push(video::OverlayLabel::new("MOTION EVENT", video::OverlayColor::White));
    }
    labels
}

fn overlay_regions_for_analysis(analysis: &EventAnalysis) -> Vec<video::OverlayRegion> {
    let mut regions: Vec<video::OverlayRegion> = analysis
        .regions
        .iter()
        .filter(|region| region.w >= 20 && region.h >= 20)
        .take(8)
        .map(|region| {
            video::OverlayRegion::new(
                region_label(region),
                region_color(analysis, region.kind),
                region.x.min(1000),
                region.y.min(1000),
                region.w.min(1000),
                region.h.min(1000),
            )
        })
        .collect();

    if regions.is_empty() && (analysis.concerning_object || analysis.concerning_behavior) {
        regions.push(video::OverlayRegion::new(
            "FACE / HEAD",
            video::OverlayColor::Red,
            570,
            100,
            320,
            360,
        ));
        regions.push(video::OverlayRegion::new(
            "CONCERNING OBJECT",
            video::OverlayColor::Red,
            360,
            80,
            280,
            300,
        ));
    }

    regions
}

fn region_label(region: &AnalysisRegion) -> String {
    if region.label.trim().is_empty() {
        match region.kind {
            RegionKind::Person => "PERSON".to_string(),
            RegionKind::Face => "FACE / HEAD".to_string(),
            RegionKind::Package => "PACKAGE".to_string(),
            RegionKind::Knife => "KNIFE / SHARP OBJECT".to_string(),
            RegionKind::Object => "OBJECT".to_string(),
            RegionKind::Gesture => "GESTURE".to_string(),
            RegionKind::Door => "DOOR".to_string(),
            RegionKind::Unknown => "REGION".to_string(),
        }
    } else {
        region.label.clone()
    }
}

fn region_color(analysis: &EventAnalysis, kind: RegionKind) -> video::OverlayColor {
    match kind {
        RegionKind::Knife => video::OverlayColor::Red,
        RegionKind::Object | RegionKind::Gesture if analysis.concerning_object || analysis.concerning_behavior => {
            video::OverlayColor::Red
        }
        RegionKind::Package => video::OverlayColor::Green,
        RegionKind::Person | RegionKind::Face if matches!(analysis.importance, Importance::High | Importance::Critical) => {
            video::OverlayColor::Yellow
        }
        _ => video::OverlayColor::White,
    }
}

fn event_id_for_analysis_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("event_")?;
    let id = rest.strip_suffix("_analysis.json")?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_string())
}

fn analysis_paths_newest_first(uploads_dir: &Path) -> Vec<(std::time::SystemTime, String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(uploads_dir) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(event_id) = event_id_for_analysis_path(&path) else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        events.push((modified, event_id, path));
    }
    events.sort_by_key(|event| std::cmp::Reverse(event.0));
    events
}

fn resolve_event_id_for_analysis(uploads_dir: &Path, requested_event_id: &str) -> Option<String> {
    if requested_event_id.eq_ignore_ascii_case("latest") || requested_event_id.eq_ignore_ascii_case("last") {
        return analysis_paths_newest_first(uploads_dir)
            .into_iter()
            .next()
            .map(|(_, event_id, _)| event_id);
    }
    (!requested_event_id.is_empty() && requested_event_id.bytes().all(|b| b.is_ascii_digit()))
        .then(|| requested_event_id.to_string())
}

fn analysis_path_for_event(uploads_dir: &Path, requested_event_id: &str) -> Option<(String, PathBuf)> {
    let event_id = resolve_event_id_for_analysis(uploads_dir, requested_event_id)?;
    let path = uploads_dir.join(format!("event_{event_id}_analysis.json"));
    path.is_file().then_some((event_id, path))
}

#[derive(Deserialize)]
struct StoredAnalysis {
    event_analysis: Option<EventAnalysis>,
}

fn read_event_analysis(path: &Path) -> Option<EventAnalysis> {
    let contents = std::fs::read_to_string(path).ok()?;
    let stored: StoredAnalysis = serde_json::from_str(&contents).ok()?;
    stored.event_analysis
}

fn analysis_for_event(uploads_dir: &Path, requested_event_id: &str) -> Option<(String, EventAnalysis)> {
    let (event_id, path) = analysis_path_for_event(uploads_dir, requested_event_id)?;
    let analysis = read_event_analysis(&path)?;
    Some((event_id, analysis))
}

fn categories_for_analysis(analysis: &EventAnalysis) -> Vec<AlertCategory> {
    let mut categories = Vec::new();
    if analysis.person {
        categories.push(AlertCategory::Visitor);
    }
    if analysis.package {
        categories.push(AlertCategory::Package);
    }
    if analysis.concerning_object || analysis.concerning_behavior {
        categories.push(AlertCategory::SecurityConcern);
    }
    if matches!(analysis.importance, Importance::High | Importance::Critical) {
        categories.push(AlertCategory::HighImportance);
    }
    if analysis.vehicle {
        categories.push(AlertCategory::Vehicle);
    }
    if analysis.animal {
        categories.push(AlertCategory::Animal);
    }
    if categories.is_empty() {
        categories.push(AlertCategory::OtherMotion);
    }
    categories
}

fn summary_message_for_event(event_id: &str, analysis: &EventAnalysis) -> String {
    let alert = Alert {
        categories: categories_for_analysis(analysis),
        analysis: analysis.clone(),
        thumbnail_path: PathBuf::from(format!("event_{event_id}_thumbnail.jpg")),
    };
    alert.message()
}

fn events_list_message(uploads_dir: &Path) -> String {
    let events = analysis_paths_newest_first(uploads_dir);
    if events.is_empty() {
        return "No analyzed FRIDAY events found yet.".to_string();
    }

    let mut message = String::from("🚪 Project FRIDAY recent events");
    for (_, event_id, path) in events.into_iter().take(5) {
        let Some(analysis) = read_event_analysis(&path) else {
            continue;
        };
        let summary = if analysis.plain_description.trim().is_empty() {
            analysis.description.as_str()
        } else {
            analysis.plain_description.as_str()
        };
        message.push_str(&format!(
            "\n\n#{event_id}\nPriority: {}\n{}",
            priority_label(analysis.importance),
            summary
        ));
    }
    message.push_str("\n\nReply: summary latest, photo latest, video latest, analysis latest, or help");
    message
}

fn preferred_photo_path_for_event(uploads_dir: &Path, requested_event_id: &str) -> Option<(String, PathBuf)> {
    let event_id = resolve_event_id_for_analysis(uploads_dir, requested_event_id)?;
    for suffix in ["keyframe_1.jpg", "keyframe_0.jpg", "thumbnail.jpg"] {
        let path = uploads_dir.join(format!("event_{event_id}_{suffix}"));
        if path.is_file() {
            return Some((event_id, path));
        }
    }

    let mut keyframes = Vec::new();
    for entry in std::fs::read_dir(uploads_dir).ok()?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with(&format!("event_{event_id}_keyframe_")) && name.ends_with(".jpg") {
            keyframes.push(path);
        }
    }
    keyframes.sort();
    keyframes.into_iter().next().map(|path| (event_id, path))
}

fn help_message() -> &'static str {
    "🚪 Project FRIDAY commands\n\n\
events\nShow the last 5 analyzed events.\n\n\
summary latest\nShow the latest event summary.\n\n\
photo latest\nSend the latest event photo.\n\n\
video latest\nSend the latest event video.\n\n\
analysis latest\nShow the full AI analysis JSON.\n\n\
resend latest\nForce-send the latest video again.\n\n\
You can also use an event ID, like: video 1783957936278"
}

fn send_video_for_event(
    telegram: &TelegramNotifier,
    uploads_dir: &Path,
    requested_event_id: &str,
    sent_video_ids: &mut std::collections::HashSet<String>,
    force_resend: bool,
) {
    match playable_video_path_for_event(uploads_dir, requested_event_id) {
        Some(path) => {
            let resolved_id = event_id_for_playable_video_path(&path).unwrap_or_else(|| requested_event_id.to_string());
            if sent_video_ids.contains(&resolved_id) && !force_resend {
                let msg = format!(
                    "I already sent video for event {resolved_id}. Reply `video {resolved_id} again` if you want another copy."
                );
                if let Err(e) = telegram.send_message(&msg) {
                    println!("telegram commands: failed to send already-sent reply: {e:?}");
                }
                return;
            }
            let caption = if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_annotated.mp4") || name.contains("_locked"))
            {
                format!("Project FRIDAY locked video for event {resolved_id}")
            } else {
                format!("Project FRIDAY video for event {resolved_id}")
            };
            match telegram.send_document(&path, &caption) {
                Ok(()) => {
                    sent_video_ids.insert(resolved_id.clone());
                    println!("telegram commands: sent video for event {resolved_id}");
                }
                Err(e) => println!("telegram commands: failed to send video for event {resolved_id}: {e:?}"),
            }
        }
        None => {
            let msg = format!("I couldn't find a playable video for event {requested_event_id}.");
            if let Err(e) = telegram.send_message(&msg) {
                println!("telegram commands: failed to send missing-video reply: {e:?}");
            }
        }
    }
}

fn send_summary_for_event(telegram: &TelegramNotifier, uploads_dir: &Path, requested_event_id: &str) {
    match analysis_for_event(uploads_dir, requested_event_id) {
        Some((event_id, analysis)) => {
            if let Err(e) = telegram.send_message(&summary_message_for_event(&event_id, &analysis)) {
                println!("telegram commands: failed to send summary for event {event_id}: {e:?}");
            }
        }
        None => {
            let msg = match analysis_path_for_event(uploads_dir, requested_event_id) {
                Some((event_id, _)) => {
                    format!("AI analysis exists for event {event_id}, but it failed. The photo/video are still saved.")
                }
                None => format!("I couldn't find an analysis file for event {requested_event_id}."),
            };
            if let Err(e) = telegram.send_message(&msg) {
                println!("telegram commands: failed to send missing-summary reply: {e:?}");
            }
        }
    }
}

fn send_analysis_for_event(telegram: &TelegramNotifier, uploads_dir: &Path, requested_event_id: &str) {
    match analysis_for_event(uploads_dir, requested_event_id) {
        Some((event_id, analysis)) => {
            let json = serde_json::to_string_pretty(&analysis).unwrap_or_else(|_| "{}".to_string());
            let message = format!("Project FRIDAY analysis for event {event_id}\n\n{json}");
            if let Err(e) = telegram.send_message(&message) {
                println!("telegram commands: failed to send analysis for event {event_id}: {e:?}");
            }
        }
        None => {
            let msg = match analysis_path_for_event(uploads_dir, requested_event_id) {
                Some((event_id, path)) => format!(
                    "AI analysis failed for event {event_id}. Saved failure file: {}",
                    path.file_name().and_then(|name| name.to_str()).unwrap_or("analysis.json")
                ),
                None => format!("I couldn't find an analysis file for event {requested_event_id}."),
            };
            if let Err(e) = telegram.send_message(&msg) {
                println!("telegram commands: failed to send missing-analysis reply: {e:?}");
            }
        }
    }
}

fn send_photo_for_event(telegram: &TelegramNotifier, uploads_dir: &Path, requested_event_id: &str) {
    match preferred_photo_path_for_event(uploads_dir, requested_event_id) {
        Some((event_id, path)) => {
            let caption = format!("Project FRIDAY photo for event {event_id}");
            if let Err(e) = telegram.send_photo(&path, &caption) {
                println!("telegram commands: failed to send photo for event {event_id}: {e:?}");
            }
        }
        None => {
            let msg = format!("I couldn't find a photo for event {requested_event_id}.");
            if let Err(e) = telegram.send_message(&msg) {
                println!("telegram commands: failed to send missing-photo reply: {e:?}");
            }
        }
    }
}

fn handle_telegram_command(
    telegram: &TelegramNotifier,
    uploads_dir: &Path,
    command: TelegramCommand,
    sent_video_ids: &mut std::collections::HashSet<String>,
) {
    match command {
        TelegramCommand::Help => {
            if let Err(e) = telegram.send_message(help_message()) {
                println!("telegram commands: failed to send help: {e:?}");
            }
        }
        TelegramCommand::Events => {
            if let Err(e) = telegram.send_message(&events_list_message(uploads_dir)) {
                println!("telegram commands: failed to send events list: {e:?}");
            }
        }
        TelegramCommand::Summary(event_id) => send_summary_for_event(telegram, uploads_dir, &event_id),
        TelegramCommand::Photo(event_id) => send_photo_for_event(telegram, uploads_dir, &event_id),
        TelegramCommand::Analysis(event_id) => send_analysis_for_event(telegram, uploads_dir, &event_id),
        TelegramCommand::Video(command) => send_video_for_event(
            telegram,
            uploads_dir,
            &command.event_id,
            sent_video_ids,
            command.force_resend,
        ),
    }
}

pub fn spawn_telegram_command_listener(uploads_dir: &'static str) {
    let Ok(token) = std::env::var("TELEGRAM_BOT_TOKEN") else {
        println!("telegram commands: disabled (TELEGRAM_BOT_TOKEN not set)");
        return;
    };
    let Ok(chat_id) = std::env::var("TELEGRAM_CHAT_ID") else {
        println!("telegram commands: disabled (TELEGRAM_CHAT_ID not set)");
        return;
    };
    if token.trim().is_empty() || chat_id.trim().is_empty() {
        println!("telegram commands: disabled (empty Telegram env var)");
        return;
    }
    let Ok(allowed_chat_id) = chat_id.parse::<i64>() else {
        println!("telegram commands: disabled (TELEGRAM_CHAT_ID is not an integer)");
        return;
    };

    let telegram = TelegramNotifier::new(token, chat_id, TELEGRAM_TIMEOUT);
    let uploads_dir = PathBuf::from(uploads_dir);
    std::thread::spawn(move || {
        println!("telegram commands: listening for help/events/summary/photo/video/analysis");
        let mut offset = None;
        let mut sent_video_ids = std::collections::HashSet::new();
        loop {
            match telegram.get_updates(offset) {
                Ok(updates) => {
                    for update in updates {
                        offset = Some(update.update_id + 1);
                        if let Some(callback) = update.callback_query {
                            let Some(message) = callback.message else {
                                continue;
                            };
                            if message.chat.id != allowed_chat_id {
                                println!("telegram commands: ignored callback from unauthorized chat {}", message.chat.id);
                                continue;
                            }
                            let Some(data) = callback.data else {
                                continue;
                            };
                            let Some(event_id) = parse_video_callback(&data) else {
                                continue;
                            };
                            let _ = telegram.answer_callback_query(&callback.id, "Checking video...");
                            send_video_for_event(&telegram, &uploads_dir, &event_id, &mut sent_video_ids, false);
                            continue;
                        }

                        if let Some(message) = update.message {
                            if message.chat.id != allowed_chat_id {
                                println!("telegram commands: ignored message from unauthorized chat {}", message.chat.id);
                                continue;
                            }
                            let Some(text) = message.text else {
                                continue;
                            };
                            let Some(command) = parse_telegram_command(&text) else {
                                continue;
                            };
                            handle_telegram_command(&telegram, &uploads_dir, command, &mut sent_video_ids);
                        }
                    }
                }
                Err(e) => {
                    println!("telegram commands: getUpdates failed: {e:?}");
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        }
    });
}

pub fn maybe_notify(
    policy: &NotificationPolicy,
    notifier: &dyn Notifier,
    analysis: &EventAnalysis,
    thumbnail_path: &Path,
    analysis_path: &Path,
) {
    let Some(alert) = policy.alert_for(analysis, thumbnail_path, analysis_path) else {
        println!(
            "notification: skipped non-actionable event (person={}, package={}, vehicle={}, animal={}, importance={:?})",
            analysis.person, analysis.package, analysis.vehicle, analysis.animal, analysis.importance
        );
        return;
    };

    match notifier.notify(&alert) {
        Ok(()) => println!("notification({}): sent", notifier.name()),
        Err(e) => println!("notification({}): failed: {e:?}", notifier.name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analysis(
        person: bool,
        package: bool,
        vehicle: bool,
        animal: bool,
        importance: Importance,
    ) -> EventAnalysis {
        EventAnalysis {
            person,
            package,
            vehicle,
            animal,
            concerning_object: false,
            concerning_behavior: false,
            description: "A visitor has been detected at your door.".to_string(),
            plain_description: "A person is standing near the door.".to_string(),
            notable_actions: vec!["standing near the door".to_string()],
            concerning_details: vec![],
            likely_intent: "visitor".to_string(),
            recommended_action: "Check the video if needed.".to_string(),
            friday_comment: String::new(),
            importance,
            confidence: 91,
            reason: vec!["A person stayed near the door.".to_string()],
            timeline: vec!["Motion started.".to_string(), "Person remained near the door.".to_string()],
            regions: vec![
                AnalysisRegion {
                    label: "face/head".to_string(),
                    kind: RegionKind::Face,
                    x: 570,
                    y: 100,
                    w: 300,
                    h: 300,
                    confidence: 80,
                },
                AnalysisRegion {
                    label: "person".to_string(),
                    kind: RegionKind::Person,
                    x: 470,
                    y: 40,
                    w: 450,
                    h: 760,
                    confidence: 75,
                },
            ],
        }
    }

    #[test]
    fn default_policy_notifies_for_person_package_or_high_importance() {
        let policy = NotificationPolicy::default();
        let thumb = Path::new("event_1_thumbnail.jpg");
        let out = Path::new("event_1_analysis.json");

        assert!(policy.alert_for(&analysis(true, false, false, false, Importance::Medium), thumb, out).is_some());
        assert!(policy.alert_for(&analysis(false, true, false, false, Importance::Medium), thumb, out).is_some());
        assert!(policy.alert_for(&analysis(false, false, false, false, Importance::High), thumb, out).is_some());
    }

    #[test]
    fn default_policy_skips_low_or_medium_empty_motion() {
        let policy = NotificationPolicy::default();
        let thumb = Path::new("event_1_thumbnail.jpg");
        let out = Path::new("event_1_analysis.json");

        assert!(policy.alert_for(&analysis(false, false, false, false, Importance::Low), thumb, out).is_none());
        assert!(policy.alert_for(&analysis(false, false, false, false, Importance::Medium), thumb, out).is_none());
    }

    #[test]
    fn vehicle_and_animal_are_opt_in() {
        let thumb = Path::new("event_1_thumbnail.jpg");
        let out = Path::new("event_1_analysis.json");
        let default = NotificationPolicy::default();

        assert!(default.alert_for(&analysis(false, false, true, false, Importance::Medium), thumb, out).is_none());
        assert!(default.alert_for(&analysis(false, false, false, true, Importance::Medium), thumb, out).is_none());

        let policy = NotificationPolicy {
            notify_vehicle: true,
            notify_animal: true,
            ..NotificationPolicy::default()
        };
        assert!(policy.alert_for(&analysis(false, false, true, false, Importance::Medium), thumb, out).is_some());
        assert!(policy.alert_for(&analysis(false, false, false, true, Importance::Medium), thumb, out).is_some());
    }

    #[test]
    fn alert_message_includes_paths_and_description() {
        let policy = NotificationPolicy::default();
        let alert = policy
            .alert_for(
                &analysis(true, false, false, false, Importance::High),
                Path::new("/tmp/event_thumbnail.jpg"),
                Path::new("/tmp/event_analysis.json"),
            )
            .unwrap();

        let message = alert.message();
        assert!(message.contains("Project FRIDAY"));
        assert!(message.contains("A person is standing near the door."));
        assert!(message.contains("Priority: High"));
        assert!(message.contains("Confidence: 91%"));
        assert!(message.contains("Notable:"));
        assert!(message.contains("Likely intent: visitor"));
        assert!(message.contains("Recommended action: Check the video if needed."));
        assert!(message.contains("Reason:"));
        assert!(message.contains("Timeline:"));
        assert!(!message.contains("/tmp/event_thumbnail.jpg"));
        assert!(!message.contains("/tmp/event_analysis.json"));
    }

    #[test]
    fn alert_message_includes_model_friday_comment() {
        let policy = NotificationPolicy::default();
        let mut analysis = analysis(true, false, false, false, Importance::High);
        analysis.friday_comment = "Bold choice, considering the camera keeps receipts.".to_string();
        let alert = policy
            .alert_for(
                &analysis,
                Path::new("/tmp/event_1783953798101_thumbnail.jpg"),
                Path::new("/tmp/event_1783953798101_analysis.json"),
            )
            .unwrap();

        let message = alert.message();
        assert!(message.contains(
            "FRIDAY says: Bold choice, considering the camera keeps receipts."
        ));
    }

    #[test]
    fn alert_message_adds_safe_fallback_friday_comment_for_concerns() {
        let policy = NotificationPolicy::default();
        let mut analysis = analysis(true, false, false, false, Importance::Medium);
        analysis.concerning_behavior = true;
        let alert = policy
            .alert_for(
                &analysis,
                Path::new("/tmp/event_1783953798101_thumbnail.jpg"),
                Path::new("/tmp/event_1783953798101_analysis.json"),
            )
            .unwrap();

        let message = alert.message();
        assert!(message.contains(
            "FRIDAY says: Bold choice, considering the camera keeps receipts."
        ));
    }

    #[test]
    fn photo_caption_is_shortened_when_full_alert_is_too_long() {
        let mut analysis = analysis(true, false, false, false, Importance::High);
        analysis.plain_description = "A person is standing very close to the door camera while holding an object near the lens and continuing to move around the doorway, with enough detail in this sentence to make the complete notification much longer than a Telegram photo caption should carry comfortably.".repeat(3);
        analysis.notable_actions = vec![
            "standing close to the camera".to_string(),
            "holding an object near the lens".to_string(),
            "moving around the doorway".to_string(),
            "remaining in view for several seconds".to_string(),
        ];
        analysis.reason = vec![
            "The subject remains close to the door camera for several frames.".to_string(),
            "The event includes visible hand movement near the recording device.".to_string(),
            "The clip contains enough context that the full alert should be sent separately.".to_string(),
        ];
        analysis.timeline = vec![
            "Motion begins near the doorway.".to_string(),
            "The person moves closer to the camera.".to_string(),
            "The person remains in frame while moving.".to_string(),
            "The person leaves or becomes less visible.".to_string(),
        ];

        let alert = Alert {
            categories: vec![AlertCategory::Visitor, AlertCategory::HighImportance],
            analysis,
            thumbnail_path: PathBuf::from("/tmp/event_1783953798101_alert.jpg"),
        };
        let full_message = alert.message();
        let caption = photo_caption_for_alert(&alert, &full_message);

        assert!(full_message.chars().count() > TELEGRAM_PHOTO_CAPTION_SOFT_LIMIT);
        assert!(caption.chars().count() <= TELEGRAM_PHOTO_CAPTION_SOFT_LIMIT);
        assert!(caption.contains("Event #1783953798101"));
        assert!(caption.contains("Full details sent below."));
    }

    #[test]
    fn alert_message_includes_event_id_and_video_command() {
        let policy = NotificationPolicy::default();
        let alert = policy
            .alert_for(
                &analysis(true, false, false, false, Importance::High),
                Path::new("/tmp/event_1783953798101_thumbnail.jpg"),
                Path::new("/tmp/event_1783953798101_analysis.json"),
            )
            .unwrap();

        let message = alert.message();
        assert!(message.contains("Event #1783953798101"));
        assert!(message.contains("video latest"));
    }

    #[test]
    fn parses_video_command_strictly() {
        assert_eq!(
            parse_video_command("video 1783953798101"),
            Some(VideoCommand {
                event_id: "1783953798101".to_string(),
                force_resend: false
            })
        );
        assert_eq!(
            parse_video_command("VIDEO 123"),
            Some(VideoCommand {
                event_id: "123".to_string(),
                force_resend: false
            })
        );
        assert_eq!(
            parse_video_command("video latest"),
            Some(VideoCommand {
                event_id: "latest".to_string(),
                force_resend: false
            })
        );
        assert_eq!(
            parse_video_command("video last again"),
            Some(VideoCommand {
                event_id: "latest".to_string(),
                force_resend: true
            })
        );
        assert_eq!(
            parse_video_command("video 123 resend"),
            Some(VideoCommand {
                event_id: "123".to_string(),
                force_resend: true
            })
        );
        assert_eq!(parse_video_command("video abc"), None);
        assert_eq!(parse_video_command("video 123 extra"), None);
        assert_eq!(parse_video_command("hello 123"), None);
    }

    #[test]
    fn parses_help_and_lookup_commands() {
        assert_eq!(parse_telegram_command("help"), Some(TelegramCommand::Help));
        assert_eq!(parse_telegram_command("/start"), Some(TelegramCommand::Help));
        assert_eq!(parse_telegram_command("commands"), Some(TelegramCommand::Help));
        assert_eq!(parse_telegram_command("events"), Some(TelegramCommand::Events));
        assert_eq!(
            parse_telegram_command("summary latest"),
            Some(TelegramCommand::Summary("latest".to_string()))
        );
        assert_eq!(
            parse_telegram_command("photo 1783957936278"),
            Some(TelegramCommand::Photo("1783957936278".to_string()))
        );
        assert_eq!(
            parse_telegram_command("analysis"),
            Some(TelegramCommand::Analysis("latest".to_string()))
        );
        assert_eq!(
            parse_telegram_command("resend latest"),
            Some(TelegramCommand::Video(VideoCommand {
                event_id: "latest".to_string(),
                force_resend: true
            }))
        );
        assert_eq!(parse_telegram_command("summary abc"), None);
    }

    #[test]
    fn events_list_uses_latest_analysis_files() {
        let dir = std::env::temp_dir().join(format!(
            "camera_server_test_notify_events_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let stored = serde_json::json!({
            "event_analysis": analysis(true, false, false, false, Importance::High),
            "identity": {"status": "not_enabled", "known_person_id": null, "display_name": null, "confidence": null},
            "ai": {"provider": "ollama", "model": "qwen3.5:4b", "status": "success"}
        });
        std::fs::write(
            dir.join("event_1783957936278_analysis.json"),
            serde_json::to_string_pretty(&stored).unwrap(),
        )
        .unwrap();

        let message = events_list_message(&dir);
        assert!(message.contains("Project FRIDAY recent events"));
        assert!(message.contains("#1783957936278"));
        assert!(message.contains("Priority: High"));
        assert!(message.contains("A person is standing near the door."));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn security_concern_notifies_even_if_importance_is_medium() {
        let policy = NotificationPolicy::default();
        let mut event = analysis(true, false, false, false, Importance::Medium);
        event.concerning_object = true;

        let alert = policy
            .alert_for(
                &event,
                Path::new("/tmp/event_1783953798101_thumbnail.jpg"),
                Path::new("/tmp/event_1783953798101_analysis.json"),
            )
            .unwrap();

        assert!(alert.categories.contains(&AlertCategory::SecurityConcern));
        assert!(alert.message().contains("security concern"));
    }

    #[test]
    fn parses_send_video_callback_data() {
        assert_eq!(parse_video_callback("video:1783953798101"), Some("1783953798101".to_string()));
        assert_eq!(parse_video_callback("video:abc"), None);
        assert_eq!(parse_video_callback("other:1783953798101"), None);
    }

    #[test]
    fn resolves_video_path_only_for_digit_event_ids() {
        let dir = std::env::temp_dir().join(format!(
            "camera_server_test_notify_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("event_123_video.bin");
        std::fs::write(&video, b"video").unwrap();

        assert!(playable_video_path_for_event(&dir, "123").is_none(), "fake raw video cannot convert to mp4");
        assert_eq!(playable_video_path_for_event(&dir, "../123"), None);
        assert_eq!(playable_video_path_for_event(&dir, "123_thumbnail"), None);
        assert_eq!(playable_video_path_for_event(&dir, "999"), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolves_existing_mp4_without_needing_raw_conversion() {
        let dir = std::env::temp_dir().join(format!(
            "camera_server_test_notify_mp4_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mp4 = dir.join("event_123_video.mp4");
        std::fs::write(&mp4, b"mp4").unwrap();

        assert_eq!(playable_video_path_for_event(&dir, "123"), Some(mp4));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolves_event_id_from_reactive_and_older_locked_videos() {
        assert_eq!(
            event_id_for_playable_video_path(Path::new("event_123_locked_reactive_threat.mp4")),
            Some("123".to_string())
        );
        assert_eq!(
            event_id_for_playable_video_path(Path::new("event_123_locked_stable_threat.mp4")),
            Some("123".to_string())
        );
        assert_eq!(
            event_id_for_playable_video_path(Path::new("event_not-numeric_locked_reactive_normal.mp4")),
            None
        );
    }
}
