//! The actions Jev can choose from: a built-in set for macOS plus the user's
//! own commands from `voice_commands.json`.
//!
//! Every action carries the words Jev reads (`what`, and `not_for` to steer
//! look-alike requests to the right sibling), the kind of argument it needs
//! (which decides the candidates code proposes), and how to run it.

use super::context::DesktopContext;
use serde::Deserialize;
use std::path::Path;

/// Jev Choice takes at most 255 options; one is reserved for `none`.
pub const MAX_ACTIONS: usize = 254;

/// Written by "Edit custom commands" when the file doesn't exist yet, so the
/// user starts from a working example rather than an empty file.
pub const CUSTOM_COMMANDS_TEMPLATE: &str = r#"{
  "commands": [
    {
      "id": "open_downloads",
      "title": "Open Downloads",
      "description": "Open the Downloads folder in Finder",
      "shell": "open ~/Downloads"
    }
  ]
}
"#;

/// What kind of argument an action needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArgKind {
    #[default]
    None,
    /// An application name, chosen by Jev from the installed and running apps.
    App,
    /// A span of the utterance, chosen by Jev from spans code cut out of it.
    Text,
    /// A number parsed from the utterance by code.
    Number,
    /// One of the frontmost app's menu commands, chosen by Jev from the ones
    /// code read. Built-in only: custom commands can't ask for it.
    #[serde(skip)]
    Menu,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Runner {
    /// AppleScript source. The argument, if any, is the property `arg`.
    AppleScript(String),
    /// Lua for Hammerspoon's `hs` CLI. The argument, if any, is the local `arg`.
    Hammerspoon(String),
    /// Run with `/bin/sh -c`. The argument, if any, is `$1`.
    Shell(String),
    /// Open a URL; `{text}` is replaced by the percent-encoded argument.
    OpenUrl(String),
    /// Open a site by name or address, jumping to the top result for a name.
    OpenWebsite,
    /// Launch or bring forward an app by name.
    OpenApp,
    /// Paste the argument into the frontmost app, exactly as dictation would.
    TypeText,
    /// Press the frontmost app's menu item whose label is the argument.
    MenuItem,
    /// Click the on-screen item Jev picks in a second request, by name.
    ClickElement,
    /// Click where the mouse pointer is, without moving it.
    ClickPointer,
}

#[derive(Debug, Clone)]
pub struct Action {
    pub id: String,
    /// Short label for the overlay and logs, e.g. "Open app".
    pub title: String,
    pub what: String,
    pub not_for: Option<String>,
    pub arg: ArgKind,
    pub runner: Runner,
}

fn action(id: &str, title: &str, what: &str, arg: ArgKind, runner: Runner) -> Action {
    Action {
        id: id.to_string(),
        title: title.to_string(),
        what: what.to_string(),
        not_for: None,
        arg,
        runner,
    }
}

impl Action {
    fn not_for(mut self, look_alike: &str) -> Self {
        self.not_for = Some(look_alike.to_string());
        self
    }
}

fn applescript(source: &str) -> Runner {
    Runner::AppleScript(source.to_string())
}

fn using_modifiers(modifiers: &[&str]) -> String {
    if modifiers.is_empty() {
        String::new()
    } else {
        let downs: Vec<String> = modifiers.iter().map(|m| format!("{m} down")).collect();
        format!(" using {{{}}}", downs.join(", "))
    }
}

/// A shortcut typed into the frontmost app.
fn keystroke(key: &str, modifiers: &[&str]) -> Runner {
    Runner::AppleScript(format!(
        "tell application \"System Events\" to keystroke \"{key}\"{}",
        using_modifiers(modifiers)
    ))
}

/// A non-character key (Return, Escape, arrows...) by virtual key code.
fn key_code(code: u16, modifiers: &[&str]) -> Runner {
    Runner::AppleScript(format!(
        "tell application \"System Events\" to key code {code}{}",
        using_modifiers(modifiers)
    ))
}

fn hammerspoon_window(body: &str) -> Runner {
    Runner::Hammerspoon(format!(
        "local w = hs.window.focusedWindow()\nif not w then error(\"no focused window\") end\n{body}"
    ))
}

/// Media keys go through Hammerspoon when it's there, so they reach whatever
/// is playing (a browser tab included). Without it, AppleScript drives Spotify
/// when it's running and Music otherwise. Spotify is only named when it's
/// installed: AppleScript resolves an app's dictionary at compile time.
fn media(hammerspoon: bool, key: &str, applescript_command: &str, spotify: bool) -> Runner {
    if hammerspoon {
        return Runner::Hammerspoon(format!(
            "hs.eventtap.event.newSystemKeyEvent(\"{key}\", true):post()\nhs.eventtap.event.newSystemKeyEvent(\"{key}\", false):post()"
        ));
    }
    if spotify {
        Runner::AppleScript(format!(
            "if application \"Spotify\" is running then\n\ttell application \"Spotify\" to {applescript_command}\nelse\n\ttell application \"Music\" to {applescript_command}\nend if"
        ))
    } else {
        Runner::AppleScript(format!(
            "tell application \"Music\" to {applescript_command}"
        ))
    }
}

/// The built-in actions for this desktop. Window tiling needs Hammerspoon and
/// is left out without it, rather than offered and then failing.
pub fn builtin(ctx: &DesktopContext) -> Vec<Action> {
    use ArgKind::{App, Menu, Number, Text};
    let hs = ctx.hammerspoon_cli.is_some();
    let spotify = ctx.installed_apps.iter().any(|app| app == "Spotify");
    let none = ArgKind::None;

    let mut actions = vec![
        // Apps
        action(
            "open_app",
            "Open app",
            "Open, launch, switch to, or bring up an application installed on this Mac",
            App,
            Runner::OpenApp,
        )
        .not_for("A website such as YouTube or Gmail when no app by that name is installed (open_website)"),
        action(
            "quit_app",
            "Quit app",
            "Quit an application entirely",
            App,
            applescript("if application arg is running then tell application arg to quit"),
        )
        .not_for("Closing only the current tab or window (close_tab_or_window)"),
        action(
            "hide_app",
            "Hide app",
            "Hide an application's windows without quitting it",
            App,
            applescript(
                "tell application \"System Events\" to set visible of application process arg to false",
            ),
        ),
        // Windows
        action(
            "window_minimize",
            "Minimize window",
            "Minimize the current window to the Dock",
            none,
            keystroke("m", &["command"]),
        ),
        action(
            "window_fullscreen",
            "Full screen",
            "Enter or leave macOS full screen for the current window",
            none,
            keystroke("f", &["control", "command"]),
        ),
        action(
            "close_tab_or_window",
            "Close tab or window",
            "Close the current tab or window",
            none,
            keystroke("w", &["command"]),
        )
        .not_for("Quitting a whole application (quit_app)"),
        // System
        action(
            "volume_up",
            "Volume up",
            "Turn the sound up or make it louder",
            none,
            applescript(
                "set newVolume to (output volume of (get volume settings)) + 10\nif newVolume > 100 then set newVolume to 100\nset volume output volume newVolume\nset volume output muted false",
            ),
        ),
        action(
            "volume_down",
            "Volume down",
            "Turn the sound down or make it quieter",
            none,
            applescript(
                "set newVolume to (output volume of (get volume settings)) - 10\nif newVolume < 0 then set newVolume to 0\nset volume output volume newVolume",
            ),
        ),
        action(
            "set_volume",
            "Set volume",
            "Set the volume to a specific level or percentage",
            Number,
            applescript(
                "set newVolume to arg as integer\nif newVolume > 100 then set newVolume to 100\nset volume output volume newVolume\nset volume output muted false",
            ),
        ),
        action(
            "mute",
            "Mute",
            "Mute the sound",
            none,
            applescript("set volume output muted true"),
        ),
        action(
            "unmute",
            "Unmute",
            "Unmute the sound",
            none,
            applescript("set volume output muted false"),
        ),
        action(
            "lock_screen",
            "Lock screen",
            "Lock the screen or the computer",
            none,
            keystroke("q", &["control", "command"]),
        ),
        action(
            "sleep_display",
            "Sleep display",
            "Turn off or sleep the display",
            none,
            Runner::Shell("pmset displaysleepnow".to_string()),
        ),
        action(
            "toggle_dark_mode",
            "Toggle dark mode",
            "Switch between dark mode and light mode",
            none,
            applescript(
                "tell application \"System Events\" to tell appearance preferences to set dark mode to not dark mode",
            ),
        ),
        action(
            "screenshot",
            "Screenshot",
            "Take a screenshot or screen recording",
            none,
            keystroke("5", &["command", "shift"]),
        ),
        // Media
        action(
            "media_play_pause",
            "Play/pause",
            "Play, pause, or resume music or other media",
            none,
            media(hs, "PLAY", "playpause", spotify),
        ),
        action(
            "media_next",
            "Next track",
            "Skip to the next song or track",
            none,
            media(hs, "NEXT", "next track", spotify),
        ),
        action(
            "media_previous",
            "Previous track",
            "Go back to the previous song or track",
            none,
            media(hs, "PREVIOUS", "previous track", spotify),
        ),
        // Web
        action(
            "web_search",
            "Web search",
            "Search the web for something",
            Text,
            Runner::OpenUrl("https://www.google.com/search?q={text}".to_string()),
        )
        .not_for("Opening a specific site by name or address (open_website)"),
        action(
            "open_website",
            "Open website",
            "Open a website or web page by its name or address",
            Text,
            Runner::OpenWebsite,
        )
        .not_for("An application installed on this Mac (open_app), or a link on the current page (click_element)"),
        action(
            "new_tab",
            "New tab",
            "Open a new browser tab",
            none,
            keystroke("t", &["command"]),
        ),
        action(
            "reopen_tab",
            "Reopen tab",
            "Reopen the tab that was just closed",
            none,
            keystroke("t", &["command", "shift"]),
        ),
        action(
            "reload_page",
            "Reload page",
            "Reload or refresh the current page",
            none,
            keystroke("r", &["command"]),
        ),
        action(
            "go_back",
            "Back",
            "Go back to the previous page",
            none,
            keystroke("[", &["command"]),
        ),
        action(
            "go_forward",
            "Forward",
            "Go forward to the next page",
            none,
            keystroke("]", &["command"]),
        ),
        action(
            "next_tab",
            "Next tab",
            "Switch to the next tab",
            none,
            key_code(48, &["control"]),
        ),
        action(
            "previous_tab",
            "Previous tab",
            "Switch to the previous tab",
            none,
            key_code(48, &["control", "shift"]),
        ),
        // Clicking in the front window
        action(
            "click_element",
            "Click",
            "Click, press, open or select something visible in the front window by its name or text: a link, button, tab, checkbox or text field, as in 'click Octopus', 'open the References link', 'press Sign in' or 'click the search box'",
            none,
            Runner::ClickElement,
        )
        .not_for("A command from the app's menu bar, switching apps, or 'click this' or 'click here' without saying what to click (click_pointer)"),
        action(
            "click_pointer",
            "Click this",
            "Click whatever is under the mouse pointer, when the user says 'click this', 'click here' or 'click that' without naming it",
            none,
            Runner::ClickPointer,
        ),
        // Editing
        action(
            "undo",
            "Undo",
            "Undo the last change or the last text typed, e.g. 'undo' or 'scratch that'",
            none,
            keystroke("z", &["command"]),
        ),
        action(
            "redo",
            "Redo",
            "Redo the change that was just undone",
            none,
            keystroke("z", &["command", "shift"]),
        ),
        action(
            "copy",
            "Copy",
            "Copy the selection",
            none,
            keystroke("c", &["command"]),
        ),
        action(
            "cut",
            "Cut",
            "Cut the selection",
            none,
            keystroke("x", &["command"]),
        ),
        action(
            "paste",
            "Paste",
            "Paste what is on the clipboard",
            none,
            keystroke("v", &["command"]),
        ),
        action(
            "select_all",
            "Select all",
            "Select everything",
            none,
            keystroke("a", &["command"]),
        ),
        action(
            "save",
            "Save",
            "Save the current document or file",
            none,
            keystroke("s", &["command"]),
        ),
        action(
            "find",
            "Find",
            "Find text in the current page or document",
            none,
            keystroke("f", &["command"]),
        )
        .not_for("Searching the web (web_search)"),
        action(
            "press_enter",
            "Enter",
            "Press Enter or Return, for example to send a message or submit",
            none,
            key_code(36, &[]),
        ),
        action(
            "press_escape",
            "Escape",
            "Press Escape, for example to cancel or dismiss a dialog",
            none,
            key_code(53, &[]),
        ),
        action(
            "delete_word",
            "Delete word",
            "Delete the previous word",
            none,
            key_code(51, &["option"]),
        ),
        action(
            "page_down",
            "Page down",
            "Scroll down a page",
            none,
            key_code(121, &[]),
        ),
        action(
            "page_up",
            "Page up",
            "Scroll up a page",
            none,
            key_code(116, &[]),
        ),
        action(
            "type_text",
            "Type",
            "Type out the given words literally, when told to 'type' or 'write' something",
            Text,
            Runner::TypeText,
        )
        .not_for("Dictated sentences that do not start with an instruction to type"),
    ];

    if hs {
        actions.extend([
            action(
                "window_left",
                "Window left",
                "Move the current window to the left half of the screen",
                none,
                hammerspoon_window("w:moveToUnit(hs.layout.left50)"),
            ),
            action(
                "window_right",
                "Window right",
                "Move the current window to the right half of the screen",
                none,
                hammerspoon_window("w:moveToUnit(hs.layout.right50)"),
            ),
            action(
                "window_maximize",
                "Maximize window",
                "Make the current window fill the screen, without macOS full screen",
                none,
                hammerspoon_window("w:maximize()"),
            )
            .not_for("Entering macOS full screen (window_fullscreen)"),
            action(
                "window_center",
                "Center window",
                "Center the current window on the screen",
                none,
                hammerspoon_window("w:centerOnScreen()"),
            ),
            action(
                "window_next_screen",
                "Window to next display",
                "Move the current window to the other display or monitor",
                none,
                hammerspoon_window("w:moveToScreen(w:screen():next())"),
            ),
        ]);
    }

    // The frontmost app's own menus: its views, panels, mailboxes and
    // features, whatever the app is.
    if !ctx.menu_items.is_empty() {
        actions.push(
            action(
                "menu_command",
                "Menu command",
                "Use one of the frontmost app's own menu commands listed in `menu_commands`: go somewhere in the app (a mailbox, view, panel or page), show or hide part of it, or use one of its features",
                Menu,
                Runner::MenuItem,
            )
            .not_for("Something another listed action does directly, such as switching apps, new tab, undo or copy, or a link or button inside the window (click_element)"),
        );
    }

    actions
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomCommandsFile {
    #[serde(default)]
    commands: Vec<CustomCommand>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomCommand {
    id: String,
    #[serde(default)]
    title: Option<String>,
    description: String,
    #[serde(default)]
    not_for: Option<String>,
    #[serde(default)]
    argument: ArgKind,
    #[serde(default)]
    applescript: Option<String>,
    #[serde(default)]
    hammerspoon: Option<String>,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

impl CustomCommand {
    fn into_action(self) -> Result<Action, String> {
        let id = self.id.trim().to_string();
        if id.is_empty()
            || id == "none"
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "command id '{id}' must be letters, digits, '_' or '-' (and not 'none')"
            ));
        }
        if self.description.trim().is_empty() {
            return Err(format!("command '{id}' needs a description"));
        }

        let mut runners = [
            self.applescript.map(Runner::AppleScript),
            self.hammerspoon.map(Runner::Hammerspoon),
            self.shell.map(Runner::Shell),
            self.url.map(Runner::OpenUrl),
        ]
        .into_iter()
        .flatten();
        let runner = match (runners.next(), runners.next()) {
            (Some(runner), None) => runner,
            _ => {
                return Err(format!(
                    "command '{id}' needs exactly one of applescript, hammerspoon, shell or url"
                ))
            }
        };

        Ok(Action {
            title: self
                .title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| id.replace(['_', '-'], " ")),
            what: self.description,
            not_for: self.not_for,
            arg: self.argument,
            runner,
            id,
        })
    }
}

/// Parse the user's command file. Errors name the offending command so they
/// can be fixed from the settings page.
pub fn parse_custom(json: &str) -> Result<Vec<Action>, String> {
    let file: CustomCommandsFile =
        serde_json::from_str(json).map_err(|e| format!("invalid JSON: {e}"))?;
    file.commands
        .into_iter()
        .map(CustomCommand::into_action)
        .collect()
}

/// A missing file is not an error: there are simply no custom commands.
pub fn load_custom(path: &Path) -> Result<Vec<Action>, String> {
    match std::fs::read_to_string(path) {
        Ok(json) => parse_custom(&json),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("could not read {}: {e}", path.display())),
    }
}

/// Custom commands replace built-ins with the same id and are appended
/// otherwise. Hammerspoon commands are dropped when its CLI isn't installed.
pub fn merge(builtin: Vec<Action>, custom: Vec<Action>, ctx: &DesktopContext) -> Vec<Action> {
    let mut actions = builtin;
    for command in custom {
        if matches!(command.runner, Runner::Hammerspoon(_)) && ctx.hammerspoon_cli.is_none() {
            continue;
        }
        match actions.iter_mut().find(|a| a.id == command.id) {
            Some(existing) => *existing = command,
            None => actions.push(command),
        }
    }
    actions.truncate(MAX_ACTIONS);
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx(hammerspoon: bool, installed: &[&str]) -> DesktopContext {
        DesktopContext {
            installed_apps: installed.iter().map(|s| s.to_string()).collect(),
            hammerspoon_cli: hammerspoon.then(|| PathBuf::from("/opt/homebrew/bin/hs")),
            ..Default::default()
        }
    }

    #[test]
    fn menu_commands_need_the_frontmost_apps_menus() {
        assert!(!builtin(&ctx(false, &[]))
            .iter()
            .any(|a| a.id == "menu_command"));

        let with_menus = DesktopContext {
            menu_items: vec![crate::voice_control::menus::MenuItem {
                path: vec!["View".into(), "Zoom In".into()],
            }],
            ..ctx(false, &[])
        };
        let action = builtin(&with_menus)
            .into_iter()
            .find(|a| a.id == "menu_command")
            .expect("menu_command is offered");
        assert_eq!(action.arg, ArgKind::Menu);
        assert_eq!(action.runner, Runner::MenuItem);
    }

    #[test]
    fn custom_commands_cannot_take_menu_arguments() {
        let err = parse_custom(
            r#"{"commands": [{"id": "x", "description": "d", "argument": "menu", "shell": "true"}]}"#,
        )
        .unwrap_err();
        assert!(err.contains("menu"), "{err}");
    }

    #[test]
    fn builtin_ids_are_unique_and_valid() {
        let actions = builtin(&ctx(true, &[]));
        let mut ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        let count = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate action ids");
        assert!(count < MAX_ACTIONS);
        assert!(!ids.contains(&"none"));
    }

    #[test]
    fn window_tiling_needs_hammerspoon() {
        let without = builtin(&ctx(false, &[]));
        assert!(without.iter().all(|a| !a.id.starts_with("window_left")));
        assert!(without
            .iter()
            .all(|a| !matches!(a.runner, Runner::Hammerspoon(_))));

        let with = builtin(&ctx(true, &[]));
        assert!(with.iter().any(|a| a.id == "window_left"));
        let play = with.iter().find(|a| a.id == "media_play_pause").unwrap();
        assert!(matches!(play.runner, Runner::Hammerspoon(_)));
    }

    #[test]
    fn media_names_spotify_only_when_installed() {
        let script = |installed: &[&str]| {
            let actions = builtin(&ctx(false, installed));
            match &actions
                .iter()
                .find(|a| a.id == "media_next")
                .unwrap()
                .runner
            {
                Runner::AppleScript(source) => source.clone(),
                other => panic!("unexpected runner {other:?}"),
            }
        };
        assert!(!script(&["Music"]).contains("Spotify"));
        assert!(script(&["Music", "Spotify"]).contains("tell application \"Spotify\""));
    }

    #[test]
    fn keystrokes_render_modifiers() {
        match keystroke("t", &["command", "shift"]) {
            Runner::AppleScript(source) => assert_eq!(
                source,
                "tell application \"System Events\" to keystroke \"t\" using {command down, shift down}"
            ),
            other => panic!("unexpected runner {other:?}"),
        }
        match key_code(36, &[]) {
            Runner::AppleScript(source) => {
                assert_eq!(source, "tell application \"System Events\" to key code 36")
            }
            other => panic!("unexpected runner {other:?}"),
        }
    }

    #[test]
    fn parses_custom_commands() {
        let actions = parse_custom(
            r#"{"commands": [
                {"id": "standup", "description": "Open my standup notes", "shell": "open ~/standup.md"},
                {"id": "focus_on", "title": "Focus", "description": "Turn on focus mode",
                 "applescript": "tell application \"Shortcuts Events\" to run shortcut \"Focus\""},
                {"id": "search_jira", "description": "Search Jira", "argument": "text",
                 "url": "https://example.atlassian.net/issues/?jql=text~\"{text}\""}
            ]}"#,
        )
        .unwrap();
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].title, "standup");
        assert_eq!(actions[0].runner, Runner::Shell("open ~/standup.md".into()));
        assert_eq!(actions[1].title, "Focus");
        assert_eq!(actions[2].arg, ArgKind::Text);
        assert!(matches!(actions[2].runner, Runner::OpenUrl(_)));
    }

    #[test]
    fn the_template_is_a_valid_command_file() {
        assert_eq!(parse_custom(CUSTOM_COMMANDS_TEMPLATE).unwrap().len(), 1);
    }

    #[test]
    fn rejects_malformed_custom_commands() {
        let two_runners = parse_custom(
            r#"{"commands": [{"id": "x", "description": "d", "shell": "a", "url": "b"}]}"#,
        );
        assert!(two_runners.unwrap_err().contains("exactly one"));

        let no_runner = parse_custom(r#"{"commands": [{"id": "x", "description": "d"}]}"#);
        assert!(no_runner.unwrap_err().contains("exactly one"));

        let bad_id =
            parse_custom(r#"{"commands": [{"id": "none", "description": "d", "shell": "true"}]}"#);
        assert!(bad_id.unwrap_err().contains("'none'"));

        let typo =
            parse_custom(r#"{"commands": [{"id": "x", "descripton": "d", "shell": "true"}]}"#);
        assert!(typo.unwrap_err().contains("invalid JSON"));
    }

    #[test]
    fn custom_commands_override_and_extend_builtins() {
        let context = ctx(false, &[]);
        let custom = parse_custom(
            r#"{"commands": [
                {"id": "web_search", "description": "Search with Kagi", "argument": "text",
                 "url": "https://kagi.com/search?q={text}"},
                {"id": "tile", "description": "Tile", "hammerspoon": "hs.alert('hi')"},
                {"id": "standup", "description": "Standup notes", "shell": "true"}
            ]}"#,
        )
        .unwrap();
        let merged = merge(builtin(&context), custom, &context);

        let search = merged.iter().find(|a| a.id == "web_search").unwrap();
        assert_eq!(search.what, "Search with Kagi");
        assert_eq!(merged.iter().filter(|a| a.id == "web_search").count(), 1);
        assert!(merged.iter().any(|a| a.id == "standup"));
        assert!(
            merged.iter().all(|a| a.id != "tile"),
            "Hammerspoon commands need the hs CLI"
        );
    }

    #[test]
    fn missing_custom_file_means_no_commands() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_custom(&dir.path().join("voice_commands.json"))
            .unwrap()
            .is_empty());
    }
}
