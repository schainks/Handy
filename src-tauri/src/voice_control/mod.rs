//! Voice commands: from speech-to-text to speech-to-computer-control.
//!
//! Handy transcribes, Jev decides, code acts. After a dictation is
//! transcribed, one TypeSafe System One ("Jev") request asks whether the
//! utterance was an instruction for the computer, which action it names, and
//! which of the candidates code proposed (apps, spans of the utterance) is the
//! argument. Code then runs the action through AppleScript, `open`, a shell
//! command, or Hammerspoon. Anything that isn't confidently a command, and any
//! failure to reach Jev, falls through to normal dictation.

#[cfg(target_os = "macos")]
mod ax;
mod candidates;
mod context;
mod elements;
mod executor;
mod jev;
mod menus;
mod registry;
mod router;

#[cfg(test)]
mod eval;

#[cfg(test)]
mod training_data;

pub use context::DesktopContext;
pub use jev::DEFAULT_MODEL;

use crate::settings::AppSettings;
use crate::utils::redact_text;
use executor::Effect;
use log::{info, warn};
use once_cell::sync::OnceCell;
use registry::{Action, ArgKind, Runner};
use router::Decision;
use std::path::PathBuf;
use std::time::Duration;
use tauri::AppHandle;

/// Commands are short. Longer utterances are dictation, and skip the Jev
/// round trip entirely.
const MAX_COMMAND_WORDS: usize = 25;
/// Jev answers in about 150-300 ms. Past this, dictation goes ahead without it.
const JEV_TIMEOUT: Duration = Duration::from_millis(2500);
pub const DEFAULT_THRESHOLD: f64 = 0.7;
/// Items at most offered to Jev for one "click …".
const MAX_CLICK_TARGETS: usize = 200;
/// How sure Jev must be of the item before it's clicked.
const MIN_TARGET_CONFIDENCE: f64 = 0.5;
/// The same for a local model, whose probability is spread over up to twenty
/// options and none, so 0.25 is already five times chance.
const LOCAL_MIN_TARGET_CONFIDENCE: f64 = 0.25;

/// How sure the model must be of an item before it's clicked.
fn min_target_confidence(profile: jev::Profile) -> f64 {
    match profile {
        jev::Profile::Jev => MIN_TARGET_CONFIDENCE,
        jev::Profile::Local => LOCAL_MIN_TARGET_CONFIDENCE,
    }
}

const CUSTOM_COMMANDS_FILE: &str = "voice_commands.json";

/// One HTTP client for the life of the app: its connection pool keeps the
/// connection to TypeSafe warm between dictations.
static HTTP: OnceCell<reqwest::Client> = OnceCell::new();

pub enum Outcome {
    /// Not a command: paste the transcription as usual.
    Dictation,
    /// A "type ..." command: paste this text instead of the transcription.
    TypeText(String),
    /// A command ran, or was recognized and failed. Nothing is pasted.
    Command {
        summary: String,
        error: Option<String>,
    },
}

/// Commands run through macOS automation, so the feature is macOS-only.
pub fn is_enabled(settings: &AppSettings) -> bool {
    settings.voice_commands_enabled && cfg!(target_os = "macos")
}

fn api_key(settings: &AppSettings) -> Option<String> {
    let from_settings = settings.voice_commands_api_key.trim();
    if !from_settings.is_empty() {
        return Some(from_settings.to_string());
    }
    std::env::var("TYPESAFE_API_KEY")
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

fn model(settings: &AppSettings) -> &str {
    match settings.voice_commands_model.trim() {
        "" => DEFAULT_MODEL,
        model => model,
    }
}

fn endpoint() -> String {
    std::env::var("TYPESAFE_ENDPOINT")
        .ok()
        .filter(|endpoint| !endpoint.trim().is_empty())
        .unwrap_or_else(|| jev::DEFAULT_ENDPOINT.to_string())
}

pub fn custom_commands_path(app: &AppHandle) -> Result<PathBuf, String> {
    crate::portable::app_data_dir(app)
        .map(|dir| dir.join(CUSTOM_COMMANDS_FILE))
        .map_err(|e| format!("Failed to get app data directory: {e}"))
}

/// Create the custom commands file from the template if it doesn't exist.
pub fn ensure_custom_commands_file(app: &AppHandle) -> Result<PathBuf, String> {
    let path = custom_commands_path(app)?;
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create {dir:?}: {e}"))?;
        }
        std::fs::write(&path, registry::CUSTOM_COMMANDS_TEMPLATE)
            .map_err(|e| format!("Failed to write {path:?}: {e}"))?;
    }
    Ok(path)
}

/// Built-in actions plus the user's commands. A broken command file is
/// reported and skipped rather than disabling voice commands.
fn available_actions(app: &AppHandle, ctx: &DesktopContext) -> Vec<Action> {
    let custom = match custom_commands_path(app).and_then(|path| registry::load_custom(&path)) {
        Ok(custom) => custom,
        Err(e) => {
            warn!("Ignoring custom voice commands: {e}");
            Vec::new()
        }
    };
    registry::merge(registry::builtin(ctx), custom, ctx)
}

/// What the settings page shows about this machine.
pub struct Status {
    pub hammerspoon: bool,
    pub builtin_actions: usize,
    pub custom_commands: usize,
    pub custom_commands_error: Option<String>,
    pub custom_commands_path: PathBuf,
}

pub fn status(app: &AppHandle) -> Result<Status, String> {
    let ctx = DesktopContext {
        hammerspoon_cli: context::hammerspoon_cli(),
        ..Default::default()
    };
    let path = custom_commands_path(app)?;
    let (custom_commands, custom_commands_error) = match registry::load_custom(&path) {
        Ok(custom) => (custom.len(), None),
        Err(e) => (0, Some(e)),
    };
    Ok(Status {
        hammerspoon: ctx.hammerspoon_cli.is_some(),
        builtin_actions: registry::builtin(&ctx).len(),
        custom_commands,
        custom_commands_error,
        custom_commands_path: path,
    })
}

fn summarize(action: &Action, arg: Option<&str>) -> String {
    match arg {
        // A menu command reads best as the item's own title.
        Some(label) if action.arg == ArgKind::Menu => {
            label.rsplit(" > ").next().unwrap_or(label).to_string()
        }
        Some(arg) if !arg.is_empty() => format!("{} · {arg}", action.title),
        _ => action.title.clone(),
    }
}

/// Decide whether `transcription` was a command and, if so, run it.
pub async fn handle(
    app: &AppHandle,
    settings: &AppSettings,
    transcription: &str,
    ctx: DesktopContext,
) -> Outcome {
    let utterance = transcription.trim();
    let words = utterance.split_whitespace().count();
    if words == 0 || words > MAX_COMMAND_WORDS {
        return Outcome::Dictation;
    }
    let profile = jev::Profile::detect(model(settings));
    // A local server needs no key; Jev does.
    let api_key = match (api_key(settings), profile) {
        (Some(key), _) => key,
        (None, jev::Profile::Local) => String::new(),
        (None, jev::Profile::Jev) => {
            warn!("Voice commands are on but no TypeSafe API key is set; pasting as dictation");
            return Outcome::Dictation;
        }
    };

    let http = match HTTP.get_or_try_init(jev::http_client) {
        Ok(http) => http.clone(),
        Err(e) => {
            warn!("Voice commands unavailable: {e}");
            return Outcome::Dictation;
        }
    };
    let client = jev::Client::new(http, &endpoint(), &api_key, model(settings), JEV_TIMEOUT)
        .with_profile(profile);
    let actions = available_actions(app, &ctx);
    let proposal = candidates::propose(utterance, &ctx, profile);

    let route = match router::route(&client, utterance, &ctx, &actions, &proposal).await {
        Ok(route) => route,
        Err(e) => {
            warn!("Voice command routing failed; pasting as dictation: {e}");
            return Outcome::Dictation;
        }
    };
    info!(
        "Voice command routing: is_command={:.2} action={} ({:.2}) app={:?} menu={:?} text={:?} in {} ms ({} menu commands offered, {}, {} input tokens)",
        route.is_command,
        route.action.as_deref().unwrap_or("none"),
        route.action_confidence,
        route.app.as_deref().map(redact_text),
        // Menu labels come from the app, not from speech.
        route.menu.as_deref(),
        route.text.as_deref().map(redact_text),
        route.latency.as_millis(),
        proposal.menus.len(),
        route.model.as_deref().unwrap_or("unknown model"),
        route.input_tokens.unwrap_or_default(),
    );

    let threshold = settings.voice_commands_threshold.clamp(0.0, 1.0);
    match router::decide(&route, &actions, &proposal, threshold) {
        Decision::Dictation => Outcome::Dictation,
        Decision::Unresolved { action, reason } => Outcome::Command {
            summary: action.title,
            error: Some(reason),
        },
        Decision::Run { action, .. } if action.runner == Runner::ClickElement => {
            click_by_name(&client, utterance, &ctx, action).await
        }
        Decision::Run { action, arg } => {
            let summary = summarize(&action, arg.as_deref());
            let result = tauri::async_runtime::spawn_blocking(move || {
                executor::run(&action, arg.as_deref(), &ctx)
            })
            .await
            .unwrap_or_else(|e| Err(format!("command task failed: {e}")));
            match result {
                Ok(Effect::Done) => Outcome::Command {
                    summary,
                    error: None,
                },
                Ok(Effect::TypeText(text)) => Outcome::TypeText(text),
                Err(error) => Outcome::Command {
                    summary,
                    error: Some(error),
                },
            }
        }
    }
}

/// "Click Octopus": read what's clickable in the front window now, ask Jev
/// which item is meant, and press it.
async fn click_by_name(
    client: &jev::Client,
    utterance: &str,
    ctx: &DesktopContext,
    action: Action,
) -> Outcome {
    let failed = |error: String| Outcome::Command {
        summary: action.title.clone(),
        error: Some(error),
    };
    let Some(pid) = ctx.frontmost_pid else {
        return failed("no app is in front".to_string());
    };
    let started = std::time::Instant::now();
    let items = match tauri::async_runtime::spawn_blocking(move || elements::read(pid)).await {
        Ok(Ok(items)) if !items.is_empty() => items,
        Ok(Ok(_)) => return failed("found nothing to click in the front window".to_string()),
        Ok(Err(e)) => return failed(e),
        Err(e) => return failed(format!("reading the window failed: {e}")),
    };
    let read_ms = started.elapsed().as_millis();

    let names: Vec<String> = items.iter().map(|item| item.name.clone()).collect();
    let cap = match client.profile() {
        jev::Profile::Jev => MAX_CLICK_TARGETS,
        jev::Profile::Local => candidates::LOCAL_MAX_TARGETS,
    };
    let offered = elements::shortlist(&names, utterance, cap);
    let labels: Vec<String> = offered.iter().map(|&i| items[i].label.clone()).collect();
    let picked = match router::pick_target(client, utterance, ctx, &labels).await {
        Ok(picked) => picked,
        Err(e) => return failed(format!("Jev didn't answer: {e}")),
    };
    info!(
        "Click target: {:?} ({:.2}) among {} items on screen, read in {read_ms} ms, {} ms in all",
        picked.as_ref().map(|(label, _)| redact_text(label)),
        picked.as_ref().map_or(0.0, |(_, confidence)| *confidence),
        labels.len(),
        started.elapsed().as_millis(),
    );

    let Some((label, confidence)) = picked else {
        return failed("couldn't tell what to click".to_string());
    };
    if confidence < min_target_confidence(client.profile()) {
        return failed(format!("not sure what to click ({confidence:.2})"));
    }
    let Some(index) = labels.iter().position(|l| *l == label).map(|i| offered[i]) else {
        return failed("couldn't tell what to click".to_string());
    };
    let Some(item) = items.into_iter().nth(index) else {
        return failed("couldn't tell what to click".to_string());
    };
    let summary = format!("{} · {}", action.title, item.name);
    let result = tauri::async_runtime::spawn_blocking(move || elements::press(&item))
        .await
        .unwrap_or_else(|e| Err(format!("click task failed: {e}")));
    Outcome::Command {
        summary,
        error: result.err(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(arg: ArgKind) -> Action {
        Action {
            id: "x".into(),
            title: "Web search".into(),
            what: String::new(),
            not_for: None,
            arg,
            runner: Runner::MenuItem,
        }
    }

    #[test]
    fn summaries_name_what_ran() {
        assert_eq!(
            summarize(&action(ArgKind::Menu), Some("Mailbox > Go To > Inbox")),
            "Inbox"
        );
        assert_eq!(
            summarize(&action(ArgKind::Text), Some("cats")),
            "Web search · cats"
        );
        assert_eq!(summarize(&action(ArgKind::None), None), "Web search");
    }
}
