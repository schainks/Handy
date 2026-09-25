//! Live routing eval against TypeSafe, through the same request builder and
//! decision code the app uses. Not run by default (it calls the API):
//!
//! ```sh
//! cd src-tauri
//! TYPESAFE_API_KEY=... cargo test voice_control::eval -- --ignored --nocapture
//! ```
//!
//! Optional: `VOICE_COMMANDS_THRESHOLD` (default 0.7), `TYPESAFE_MODEL`.
//! The desktop is a fixed fake (so results don't depend on this machine) with
//! Hammerspoon present, so window actions are in play.
//!
//! The dictation cases deliberately contain command words ("open", "close",
//! "search", "save", "next"): the number that matters most with auto-detect on
//! the dictation key is how often dictation gets run as a command.

use super::candidates;
use super::context::DesktopContext;
use super::jev;
use super::registry;
use super::router::{self, Decision};
use std::path::PathBuf;
use std::time::Duration;

/// (utterance, expected action or None for dictation, expected argument)
const CASES: &[(&str, Option<&str>, Option<&str>)] = &[
    // Apps
    ("Open Safari.", Some("open_app"), Some("Safari")),
    ("Switch to Slack", Some("open_app"), Some("Slack")),
    (
        "Launch visual studio code",
        Some("open_app"),
        Some("Visual Studio Code"),
    ),
    ("Bring up my terminal", Some("open_app"), Some("Terminal")),
    ("Quit Spotify.", Some("quit_app"), Some("Spotify")),
    ("Close zoom completely", Some("quit_app"), Some("zoom.us")),
    ("Hide Messages", Some("hide_app"), Some("Messages")),
    // Windows
    (
        "Move this window to the left half",
        Some("window_left"),
        None,
    ),
    ("Snap it to the right", Some("window_right"), None),
    (
        "Make this window fill the screen",
        Some("window_maximize"),
        None,
    ),
    (
        "Send this to my other monitor",
        Some("window_next_screen"),
        None,
    ),
    ("Minimize this.", Some("window_minimize"), None),
    ("Close this tab", Some("close_tab_or_window"), None),
    // System and media
    ("Turn it up.", Some("volume_up"), None),
    ("Quieter please", Some("volume_down"), None),
    ("Set the volume to 30%.", Some("set_volume"), Some("30")),
    ("Mute.", Some("mute"), None),
    ("Pause the music", Some("media_play_pause"), None),
    ("Next song", Some("media_next"), None),
    ("Lock my computer", Some("lock_screen"), None),
    ("Switch to dark mode", Some("toggle_dark_mode"), None),
    ("Take a screenshot", Some("screenshot"), None),
    // Web
    (
        "Search for flights to Denver.",
        Some("web_search"),
        Some("flights to Denver"),
    ),
    (
        "Google best ramen in Oakland",
        Some("web_search"),
        Some("best ramen in Oakland"),
    ),
    ("Go to github.com", Some("open_website"), Some("github.com")),
    ("Open YouTube", Some("open_website"), Some("YouTube")),
    ("New tab", Some("new_tab"), None),
    ("Reload the page", Some("reload_page"), None),
    ("Go back", Some("go_back"), None),
    // Editing
    ("Scratch that.", Some("undo"), None),
    ("Select all", Some("select_all"), None),
    ("Copy that", Some("copy"), None),
    ("Save the file.", Some("save"), None),
    ("Press enter", Some("press_enter"), None),
    ("Type hello world", Some("type_text"), Some("hello world")),
    // Dictation that must stay dictation
    (
        "Hey Sarah, can you open the doc I sent you and add your comments by Friday?",
        None,
        None,
    ),
    (
        "I think we should close the deal before the end of the quarter.",
        None,
        None,
    ),
    ("Thanks so much for your help today.", None, None),
    ("The volume of sales went up last month.", None, None),
    (
        "Let me know if you want to grab lunch tomorrow.",
        None,
        None,
    ),
    ("Save the date, our wedding is on June 14th.", None, None),
    ("Remember to lock the door when you leave.", None, None),
    ("Next, we need to talk about the budget.", None, None),
    (
        "Search engines index billions of pages every day.",
        None,
        None,
    ),
    (
        "Open source software changed how we build products.",
        None,
        None,
    ),
    ("Quit your job and follow your dreams, he said.", None, None),
    ("Thank you.", None, None),
];

const APPS: &[&str] = &[
    "Activity Monitor",
    "App Store",
    "Calculator",
    "Calendar",
    "ChatGPT",
    "Claude",
    "Discord",
    "FaceTime",
    "Figma",
    "Finder",
    "Google Chrome",
    "Hammerspoon",
    "Handy",
    "Keynote",
    "Linear",
    "Mail",
    "Maps",
    "Messages",
    "Music",
    "Notes",
    "Notion",
    "Numbers",
    "Obsidian",
    "Pages",
    "Photos",
    "Preview",
    "Reminders",
    "Safari",
    "Slack",
    "Spotify",
    "System Settings",
    "Terminal",
    "TextEdit",
    "Visual Studio Code",
    "WhatsApp",
    "Xcode",
    "zoom.us",
];

fn fake_desktop() -> DesktopContext {
    DesktopContext {
        frontmost_app: Some("Notes".into()),
        running_apps: vec!["Notes".into(), "Slack".into(), "Safari".into()],
        installed_apps: APPS.iter().map(|app| app.to_string()).collect(),
        hammerspoon_cli: Some(PathBuf::from("/opt/homebrew/bin/hs")),
    }
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

#[test]
#[ignore = "calls the live TypeSafe API; set TYPESAFE_API_KEY and pass --ignored"]
fn live_routing_eval() {
    let api_key = std::env::var("TYPESAFE_API_KEY").expect("TYPESAFE_API_KEY is not set");
    let model = std::env::var("TYPESAFE_MODEL").unwrap_or_else(|_| jev::DEFAULT_MODEL.into());
    let threshold: f64 = std::env::var("VOICE_COMMANDS_THRESHOLD")
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(super::DEFAULT_THRESHOLD);
    let client = jev::Client::new(
        jev::http_client().unwrap(),
        jev::DEFAULT_ENDPOINT,
        &api_key,
        &model,
        Duration::from_secs(10),
    );

    let ctx = fake_desktop();
    let actions = registry::builtin(&ctx);
    let (mut commands_right, mut commands_total) = (0, 0);
    let (mut false_commands, mut dictation_total) = (0, 0);
    let mut latencies = Vec::new();

    println!(
        "threshold {threshold:.2}, model {model}, {} actions\n",
        actions.len()
    );
    for (utterance, expected_action, expected_arg) in CASES {
        let proposal = candidates::propose(utterance, &ctx);
        let route = tauri::async_runtime::block_on(router::route(
            &client, utterance, &ctx, &actions, &proposal,
        ))
        .unwrap_or_else(|e| panic!("routing '{utterance}' failed: {e}"));
        latencies.push(route.latency.as_millis());

        let (got_action, got_arg) = match router::decide(&route, &actions, &proposal, threshold) {
            Decision::Dictation => (None, None),
            Decision::Run { action, arg } => (Some(action.id), arg),
            Decision::Unresolved { action, .. } => (Some(action.id), None),
        };
        let arg_ok = match expected_arg {
            Some(expected) => got_arg
                .as_deref()
                .is_some_and(|got| got.eq_ignore_ascii_case(expected)),
            None => true,
        };
        let ok = got_action.as_deref() == *expected_action && arg_ok;

        match expected_action {
            Some(_) => {
                commands_total += 1;
                commands_right += ok as usize;
            }
            None => {
                dictation_total += 1;
                false_commands += got_action.is_some() as usize;
            }
        }
        println!(
            "{} {:<78} p(cmd)={:.2} -> {} {} [{} ms]",
            if ok { "ok  " } else { "MISS" },
            utterance,
            route.is_command,
            got_action.as_deref().unwrap_or("dictation"),
            got_arg.as_deref().unwrap_or(""),
            route.latency.as_millis(),
        );
    }

    latencies.sort_unstable();
    println!(
        "\ncommands right: {commands_right}/{commands_total}\ndictation run as a command: {false_commands}/{dictation_total}\nlatency p50 {} ms, p90 {} ms",
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.9),
    );
}
