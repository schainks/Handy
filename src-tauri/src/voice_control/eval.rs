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
//!
//! `live_menu_eval` does the same for menu commands, with Safari, Slack, Mail
//! and Calendar in front and menus modeled on theirs. `live_click_eval` runs
//! "click …" through both requests against Wikipedia's Octopus article.

use super::candidates;
use super::context::DesktopContext;
use super::elements;
use super::jev;
use super::menus::{self, RawItem};
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
        ..Default::default()
    }
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// The live client, model and threshold, from the environment. `TYPESAFE_ENDPOINT`
/// points it at another System One server, such as a local CLM.
fn live_client() -> (jev::Client, String, f64) {
    let model = std::env::var("TYPESAFE_MODEL").unwrap_or_else(|_| jev::DEFAULT_MODEL.into());
    let profile = jev::Profile::detect(&model);
    let api_key = match profile {
        jev::Profile::Jev => {
            std::env::var("TYPESAFE_API_KEY").expect("TYPESAFE_API_KEY is not set")
        }
        jev::Profile::Local => std::env::var("TYPESAFE_API_KEY").unwrap_or_default(),
    };
    let threshold: f64 = std::env::var("VOICE_COMMANDS_THRESHOLD")
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(super::DEFAULT_THRESHOLD);
    let endpoint =
        std::env::var("TYPESAFE_ENDPOINT").unwrap_or_else(|_| jev::DEFAULT_ENDPOINT.into());
    let client = jev::Client::new(
        jev::http_client().unwrap(),
        &endpoint,
        &api_key,
        &model,
        Duration::from_secs(10),
    )
    .with_profile(profile);
    (client, model, threshold)
}

#[test]
#[ignore = "calls the live TypeSafe API; set TYPESAFE_API_KEY and pass --ignored"]
fn live_routing_eval() {
    let (client, model, threshold) = live_client();

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
        let proposal = candidates::propose(utterance, &ctx, client.profile());
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

/// (frontmost app, utterance, expected menu command, or None for dictation)
const MENU_CASES: &[(&str, &str, Option<&str>)] = &[
    ("Safari", "Show my downloads", Some("View > Show Downloads")),
    (
        "Safari",
        "Bookmark this page",
        Some("Bookmarks > Add Bookmark…"),
    ),
    (
        "Safari",
        "Show all my history",
        Some("History > Show All History"),
    ),
    ("Safari", "Zoom in", Some("View > Zoom In")),
    (
        "Safari",
        "Make the text bigger",
        Some("View > Make Text Bigger"),
    ),
    (
        "Safari",
        "Open a private window",
        Some("File > New Private Window"),
    ),
    ("Safari", "Show the sidebar", Some("View > Show Sidebar")),
    (
        "Safari",
        "Add this to my reading list",
        Some("Bookmarks > Add to Reading List"),
    ),
    (
        "Safari",
        "Switch to reader mode",
        Some("View > Show Reader"),
    ),
    ("Slack", "Go to my threads", Some("Go > Threads")),
    ("Slack", "Show my activity", Some("Go > Activity")),
    ("Slack", "Open my DMs", Some("Go > DMs")),
    (
        "Slack",
        "Next unread channel",
        Some("Go > Next Unread Channel"),
    ),
    ("Slack", "Hide the sidebar", Some("View > Hide Sidebar")),
    ("Slack", "Start a new message", Some("File > New Message")),
    ("Mail", "Go to my inbox", Some("Mailbox > Go To > Inbox")),
    ("Mail", "Show my sent mail", Some("Mailbox > Go To > Sent")),
    (
        "Mail",
        "Check for new mail",
        Some("Mailbox > Get All New Mail"),
    ),
    ("Mail", "Reply to this", Some("Message > Reply")),
    ("Mail", "Forward this email", Some("Message > Forward")),
    (
        "Mail",
        "Mark this as unread",
        Some("Message > Mark > As Unread"),
    ),
    ("Mail", "Archive this message", Some("Message > Archive")),
    ("Calendar", "Show the week view", Some("View > by Week")),
    ("Calendar", "Switch to month view", Some("View > by Month")),
    ("Calendar", "Go to today", Some("View > Go to Today")),
    ("Calendar", "Create a new event", Some("File > New Event")),
    // Dictation that must stay dictation
    (
        "Mail",
        "Please reply to this by Friday so we can finalize the plan.",
        None,
    ),
    (
        "Mail",
        "Can you forward me the invoice from last month?",
        None,
    ),
    ("Slack", "Did you see the thread about the launch?", None),
    ("Slack", "I'll jump on a call in five minutes.", None),
    ("Calendar", "Let's move the meeting to next week.", None),
    (
        "Safari",
        "The history of the Roman Empire is fascinating.",
        None,
    ),
];

fn item(title: &str) -> RawItem {
    RawItem {
        title: title.into(),
        enabled: true,
        has_shortcut: false,
        submenu: None,
    }
}

/// An item with a keyboard shortcut.
fn key(title: &str) -> RawItem {
    RawItem {
        has_shortcut: true,
        ..item(title)
    }
}

fn sep() -> RawItem {
    RawItem::default()
}

fn sub(title: &str, items: Vec<RawItem>) -> RawItem {
    RawItem {
        submenu: Some(items),
        ..item(title)
    }
}

fn top(title: &str, items: Vec<RawItem>) -> (String, Vec<RawItem>) {
    (title.into(), items)
}

fn edit_menu() -> (String, Vec<RawItem>) {
    top(
        "Edit",
        vec![
            key("Undo"),
            key("Redo"),
            sep(),
            key("Cut"),
            key("Copy"),
            key("Paste"),
            key("Select All"),
            sep(),
            sub(
                "Find",
                vec![key("Find…"), key("Find Next"), key("Find Previous")],
            ),
        ],
    )
}

/// Menu bars modeled on the real apps, including the parts that must be
/// filtered out (Apple menu, Quit, history entries, window titles).
fn fake_menus(app: &str) -> Vec<(String, Vec<RawItem>)> {
    let apple = top("Apple", vec![item("About This Mac"), item("Restart…")]);
    match app {
        "Safari" => vec![
            apple,
            top(
                "Safari",
                vec![
                    item("About Safari"),
                    key("Settings…"),
                    item("Privacy Report"),
                    item("Clear History…"),
                    sep(),
                    sub("Services", vec![item("Make Sticky")]),
                    sep(),
                    key("Hide Safari"),
                    key("Quit Safari"),
                ],
            ),
            top(
                "File",
                vec![
                    key("New Window"),
                    key("New Private Window"),
                    key("New Tab"),
                    key("Open File…"),
                    key("Open Location…"),
                    sep(),
                    key("Close Window"),
                    key("Close Tab"),
                    key("Save As…"),
                    sub(
                        "Share",
                        vec![item("Mail"), item("Messages"), item("AirDrop")],
                    ),
                    sep(),
                    item("Export as PDF…"),
                    key("Print…"),
                ],
            ),
            edit_menu(),
            top(
                "View",
                vec![
                    item("Show Toolbar"),
                    item("Customize Toolbar…"),
                    sep(),
                    item("Show Tab Bar"),
                    key("Show Tab Overview"),
                    key("Show Sidebar"),
                    key("Show Downloads"),
                    sep(),
                    key("Stop"),
                    key("Reload Page"),
                    sep(),
                    key("Actual Size"),
                    key("Zoom In"),
                    key("Zoom Out"),
                    key("Make Text Bigger"),
                    key("Make Text Smaller"),
                    sep(),
                    key("Show Reader"),
                    key("Enter Full Screen"),
                ],
            ),
            top(
                "History",
                vec![
                    key("Show Start Page"),
                    key("Back"),
                    key("Forward"),
                    key("Home"),
                    sep(),
                    key("Reopen Last Closed Tab"),
                    item("Reopen All Windows from Last Session"),
                    sep(),
                    item("GitHub - schainks/Handy"),
                    item("Inbox (3) - Gmail"),
                    sep(),
                    key("Show All History"),
                    item("Clear History…"),
                ],
            ),
            top(
                "Bookmarks",
                vec![
                    key("Show Bookmarks"),
                    key("Edit Bookmarks"),
                    key("Add Bookmark…"),
                    key("Add to Reading List"),
                    sep(),
                    sub("Favorites", vec![item("Hacker News"), item("Weather")]),
                    sep(),
                    item("Recipes"),
                ],
            ),
            top(
                "Window",
                vec![
                    key("Minimize"),
                    item("Zoom"),
                    sep(),
                    key("Show Previous Tab"),
                    key("Show Next Tab"),
                    item("Move Tab to New Window"),
                    item("Merge All Windows"),
                    sep(),
                    item("Bring All to Front"),
                    sep(),
                    item("Handy PR - GitHub"),
                ],
            ),
        ],
        "Slack" => vec![
            apple,
            top(
                "Slack",
                vec![
                    item("About Slack"),
                    key("Settings…"),
                    sep(),
                    key("Hide Slack"),
                    key("Quit Slack"),
                ],
            ),
            top(
                "File",
                vec![
                    key("New Message"),
                    key("New Window"),
                    sep(),
                    sub("Workspace", vec![item("Acme"), item("Personal")]),
                    sep(),
                    key("Close Window"),
                ],
            ),
            edit_menu(),
            top(
                "View",
                vec![
                    key("Reload"),
                    sep(),
                    key("Actual Size"),
                    key("Zoom In"),
                    key("Zoom Out"),
                    sep(),
                    key("Toggle Full Screen"),
                    key("Hide Sidebar"),
                ],
            ),
            top(
                "Go",
                vec![
                    key("Back"),
                    key("Forward"),
                    sep(),
                    key("Home"),
                    key("DMs"),
                    key("Activity"),
                    key("Threads"),
                    key("Later"),
                    sep(),
                    key("Jump to…"),
                    key("Search"),
                    sep(),
                    key("Next Unread Channel"),
                    key("Previous Unread Channel"),
                    key("Next Channel"),
                    key("Previous Channel"),
                ],
            ),
            top(
                "Window",
                vec![
                    key("Minimize"),
                    item("Zoom"),
                    sep(),
                    item("general | Acme – Slack"),
                ],
            ),
        ],
        "Mail" => vec![
            apple,
            top(
                "Mail",
                vec![
                    item("About Mail"),
                    key("Settings…"),
                    item("Accounts…"),
                    sep(),
                    sub("Services", vec![item("Make Sticky")]),
                    sep(),
                    key("Hide Mail"),
                    key("Quit Mail"),
                ],
            ),
            top(
                "File",
                vec![
                    key("New Message"),
                    key("New Viewer Window"),
                    key("Open Message"),
                    key("Close"),
                    sep(),
                    key("Save As…"),
                    item("Save Attachments…"),
                    sep(),
                    key("Print…"),
                ],
            ),
            edit_menu(),
            top(
                "View",
                vec![
                    sub("Sort By", vec![item("Date"), item("From"), item("Subject")]),
                    sep(),
                    key("Hide Sidebar"),
                    item("Show Favorites Bar"),
                    sep(),
                    key("Enter Full Screen"),
                ],
            ),
            top(
                "Mailbox",
                vec![
                    item("Take All Accounts Online"),
                    sep(),
                    key("Get All New Mail"),
                    sep(),
                    sub(
                        "Go To",
                        vec![
                            key("Inbox"),
                            key("VIPs"),
                            key("Sent"),
                            key("Drafts"),
                            key("Flagged"),
                        ],
                    ),
                    sub("Move To", vec![item("Archive"), item("Receipts")]),
                    sep(),
                    item("New Mailbox…"),
                    sub("Erase Deleted Items", vec![item("In All Accounts")]),
                    key("Erase Junk Mail"),
                ],
            ),
            top(
                "Message",
                vec![
                    key("Send Again"),
                    sep(),
                    key("Reply"),
                    key("Reply All"),
                    key("Forward"),
                    key("Redirect"),
                    sep(),
                    sub(
                        "Mark",
                        vec![key("As Read"), key("As Unread"), key("As Junk Mail")],
                    ),
                    sub("Flag", vec![item("Red"), item("Orange")]),
                    key("Archive"),
                    item("Move to Junk"),
                    sep(),
                    item("Mute"),
                ],
            ),
            top(
                "Window",
                vec![
                    key("Minimize"),
                    item("Zoom"),
                    sep(),
                    key("Message Viewer"),
                    item("Activity"),
                    sep(),
                    item("Inbox — iCloud"),
                ],
            ),
        ],
        "Calendar" => vec![
            apple,
            top(
                "Calendar",
                vec![
                    item("About Calendar"),
                    key("Settings…"),
                    sep(),
                    key("Quit Calendar"),
                ],
            ),
            top(
                "File",
                vec![
                    key("New Event"),
                    key("New Calendar"),
                    item("New Calendar Subscription…"),
                    sep(),
                    item("Import…"),
                    sep(),
                    key("Close"),
                    key("Print…"),
                ],
            ),
            edit_menu(),
            top(
                "View",
                vec![
                    key("by Day"),
                    key("by Week"),
                    key("by Month"),
                    key("by Year"),
                    sep(),
                    key("Next"),
                    key("Previous"),
                    key("Go to Today"),
                    key("Go to Date…"),
                    sep(),
                    item("Show Calendar List"),
                    sep(),
                    key("Refresh Calendars"),
                ],
            ),
            top("Window", vec![key("Minimize"), item("Zoom")]),
        ],
        other => panic!("no fake menus for {other}"),
    }
}

fn desktop_with_front(app: &str) -> DesktopContext {
    DesktopContext {
        frontmost_app: Some(app.into()),
        frontmost_pid: Some(1),
        menu_items: menus::commands(&fake_menus(app)),
        ..fake_desktop()
    }
}

#[test]
fn fake_menus_keep_commands_and_drop_private_lists() {
    let labels: Vec<String> = desktop_with_front("Safari")
        .menu_items
        .iter()
        .map(|item| item.label())
        .collect();
    for expected in [
        "View > Show Downloads",
        "History > Show All History",
        "Bookmarks > Add Bookmark…",
        "Window > Show Next Tab",
    ] {
        assert!(labels.contains(&expected.to_string()), "{expected} missing");
    }
    for private in [
        "GitHub - schainks/Handy",
        "Recipes",
        "Handy PR - GitHub",
        "Hacker News",
    ] {
        assert!(
            !labels.iter().any(|label| label.ends_with(private)),
            "{private} was offered"
        );
    }
    assert!(!labels.iter().any(|label| label.contains("Quit")));
    for (app, _, expected) in MENU_CASES {
        if let Some(expected) = expected {
            let labels: Vec<String> = desktop_with_front(app)
                .menu_items
                .iter()
                .map(|item| item.label())
                .collect();
            assert!(
                labels.contains(&expected.to_string()),
                "{app}: {expected} missing"
            );
        }
    }
}

#[test]
#[ignore = "calls the live TypeSafe API; set TYPESAFE_API_KEY and pass --ignored"]
fn live_menu_eval() {
    let (client, model, threshold) = live_client();
    let (mut right, mut total) = (0, 0);
    let (mut false_commands, mut dictation_total) = (0, 0);
    let mut latencies = Vec::new();

    println!("threshold {threshold:.2}, model {model}\n");
    for (app, utterance, expected) in MENU_CASES {
        let ctx = desktop_with_front(app);
        let actions = registry::builtin(&ctx);
        let proposal = candidates::propose(utterance, &ctx, client.profile());
        let route = tauri::async_runtime::block_on(router::route(
            &client, utterance, &ctx, &actions, &proposal,
        ))
        .unwrap_or_else(|e| panic!("routing '{utterance}' failed: {e}"));
        latencies.push(route.latency.as_millis());

        let got = match router::decide(&route, &actions, &proposal, threshold) {
            Decision::Dictation => None,
            Decision::Run { action, arg } => {
                Some(format!("{} {}", action.id, arg.unwrap_or_default()))
            }
            Decision::Unresolved { action, reason } => {
                Some(format!("{} (unresolved: {reason})", action.id))
            }
        };
        let ok = match expected {
            Some(label) => got.as_deref() == Some(&format!("menu_command {label}")),
            None => got.is_none(),
        };
        match expected {
            Some(_) => {
                total += 1;
                right += ok as usize;
            }
            None => {
                dictation_total += 1;
                false_commands += got.is_some() as usize;
            }
        }
        println!(
            "{} [{app:<8}] {:<60} p(cmd)={:.2} menu p={:.2} -> {} [{} menu commands, {} ms]",
            if ok { "ok  " } else { "MISS" },
            utterance,
            route.is_command,
            route.menu_confidence,
            got.as_deref().unwrap_or("dictation"),
            proposal.menus.len(),
            route.latency.as_millis(),
        );
    }

    latencies.sort_unstable();
    println!(
        "\nmenu commands right: {right}/{total}\ndictation run as a command: {false_commands}/{dictation_total}\nlatency p50 {} ms, p90 {} ms",
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.9),
    );
}

/// What the click reader would find on Wikipedia's Octopus article in Safari:
/// Safari's controls, then the visible part of the page, in screen order.
fn wikipedia_screen() -> Vec<String> {
    let items: &[(&str, &str)] = &[
        ("Back", "button"),
        ("Forward", "button"),
        ("Show sidebar", "button"),
        ("Octopus - Wikipedia", "tab"),
        ("Share", "button"),
        ("New Tab", "button"),
        ("Main menu", "button"),
        ("Wikipedia The Free Encyclopedia", "link"),
        ("Search Wikipedia", "field"),
        ("Search", "button"),
        ("Donate", "link"),
        ("Create account", "link"),
        ("Log in", "link"),
        ("Personal tools", "button"),
        ("Main page", "link"),
        ("Contents", "link"),
        ("Current events", "link"),
        ("Random article", "link"),
        ("About Wikipedia", "link"),
        ("Contact us", "link"),
        ("(Top)", "link"),
        ("Etymology and pluralisation", "link"),
        ("Evolution", "link"),
        ("Anatomy", "link"),
        ("Intelligence", "link"),
        ("Distribution and habitat", "link"),
        ("References", "link"),
        ("Article", "link"),
        ("Talk", "link"),
        ("Read", "link"),
        ("View source", "link"),
        ("View history", "link"),
        ("Tools", "button"),
        ("For other uses, see Octopus (disambiguation)", "link"),
        ("mollusc", "link"),
        ("order", "link"),
        ("Octopoda", "link"),
        ("[1]", "link"),
        ("cephalopods", "link"),
        ("squids", "link"),
        ("cuttlefish", "link"),
        ("nautiloids", "link"),
        ("[2]", "link"),
        ("beak", "link"),
        ("siphon", "link"),
        ("chromatophores", "link"),
        ("camouflage", "link"),
        ("edit", "link"),
        ("edit", "link"),
    ];
    let items: Vec<(String, &str)> = items
        .iter()
        .map(|(name, kind)| (name.to_string(), *kind))
        .collect();
    elements::labels(&items)
}

/// (utterance, expected): the item's label, "@pointer" for "click this",
/// or None for dictation.
const CLICK_CASES: &[(&str, Option<&str>)] = &[
    ("Click cephalopods", Some("cephalopods (link)")),
    ("Open the squid link", Some("squids (link)")),
    ("Click random article", Some("Random article (link)")),
    (
        "Go to the intelligence section",
        Some("Intelligence (link)"),
    ),
    ("Click the search box", Some("Search Wikipedia (field)")),
    ("Click on references", Some("References (link)")),
    ("Open the talk page", Some("Talk (link)")),
    ("Click view history", Some("View history (link)")),
    ("Click camouflage", Some("camouflage (link)")),
    ("Press the share button", Some("Share (button)")),
    ("Click this", Some("@pointer")),
    ("Click here", Some("@pointer")),
    ("Click that one", Some("@pointer")),
    // Dictation that must stay dictation
    ("I clicked on the link you sent me yesterday.", None),
    ("Can you click through the slides before the meeting?", None),
    ("The octopus has three hearts and blue blood.", None),
];

#[test]
fn wikipedia_screen_has_every_expected_item() {
    let screen = wikipedia_screen();
    for (_, expected) in CLICK_CASES {
        if let Some(label) = expected.filter(|label| !label.starts_with('@')) {
            assert!(screen.contains(&label.to_string()), "{label} missing");
        }
    }
    assert!(screen.contains(&"edit (link) 2".to_string()));
}

#[test]
#[ignore = "calls the live TypeSafe API; set TYPESAFE_API_KEY and pass --ignored"]
fn live_click_eval() {
    let (client, model, threshold) = live_client();
    let ctx = desktop_with_front("Safari");
    let actions = registry::builtin(&ctx);
    let screen = wikipedia_screen();
    let (mut right, mut total) = (0, 0);
    let (mut false_commands, mut dictation_total) = (0, 0);
    let mut latencies = Vec::new();

    println!(
        "threshold {threshold:.2}, model {model}, {} items on screen\n",
        screen.len()
    );
    for (utterance, expected) in CLICK_CASES {
        let proposal = candidates::propose(utterance, &ctx, client.profile());
        let route = tauri::async_runtime::block_on(router::route(
            &client, utterance, &ctx, &actions, &proposal,
        ))
        .unwrap_or_else(|e| panic!("routing '{utterance}' failed: {e}"));
        let mut latency = route.latency.as_millis();

        let got = match router::decide(&route, &actions, &proposal, threshold) {
            Decision::Dictation => None,
            Decision::Run { action, .. } if action.id == "click_pointer" => {
                Some("@pointer".to_string())
            }
            Decision::Run { action, .. } if action.id == "click_element" => {
                let started = std::time::Instant::now();
                let picked = tauri::async_runtime::block_on(router::pick_target(
                    &client, utterance, &ctx, &screen,
                ))
                .unwrap_or_else(|e| panic!("picking for '{utterance}' failed: {e}"));
                latency += started.elapsed().as_millis();
                Some(match picked {
                    Some((label, confidence))
                        if confidence >= super::min_target_confidence(client.profile()) =>
                    {
                        label
                    }
                    Some((label, confidence)) => format!("unsure: {label} ({confidence:.2})"),
                    None => "nothing picked".to_string(),
                })
            }
            Decision::Run { action, arg } => {
                Some(format!("{} {}", action.id, arg.unwrap_or_default()))
            }
            Decision::Unresolved { action, reason } => {
                Some(format!("{} (unresolved: {reason})", action.id))
            }
        };
        latencies.push(latency);
        let ok = got.as_deref() == *expected;
        match expected {
            Some(_) => {
                total += 1;
                right += ok as usize;
            }
            None => {
                dictation_total += 1;
                false_commands += got.is_some() as usize;
            }
        }
        println!(
            "{} {:<55} p(cmd)={:.2} -> {} [{} ms]",
            if ok { "ok  " } else { "MISS" },
            utterance,
            route.is_command,
            got.as_deref().unwrap_or("dictation"),
            latency,
        );
    }

    latencies.sort_unstable();
    println!(
        "\nclicks right: {right}/{total}\ndictation run as a command: {false_commands}/{dictation_total}\nlatency (both requests) p50 {} ms, p90 {} ms",
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.9),
    );
}
