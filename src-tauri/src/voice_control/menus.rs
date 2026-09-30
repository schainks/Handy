//! The front app's menu bar as voice commands. Every Mac app lists what it can
//! do in its menus ("View > Show Downloads", "Mailbox > Go To > Inbox"), so
//! code reads them through the Accessibility API, Jev picks the one the
//! utterance asks for, and code presses it.
//!
//! Some menu items are left out:
//! - the Apple menu, and items that quit, log out, restart or erase;
//! - lists of recent documents, history, bookmarks and open windows, whose
//!   titles are the user's own content and would otherwise be sent to Jev.
//!   In the History, Bookmarks and Window menus only the fixed commands are
//!   kept: the first section, and items with a keyboard shortcut or a dialog.

// The filtering below is used by the macOS reader and by tests everywhere.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::HashSet;

/// A menu command, by the titles along its path from the menu bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub path: Vec<String>,
}

impl MenuItem {
    /// What Jev reads and picks, e.g. "Mailbox > Go To > Inbox".
    pub fn label(&self) -> String {
        self.path.join(" > ")
    }

    /// The item's own title, for the overlay.
    pub fn title(&self) -> &str {
        self.path.last().map(String::as_str).unwrap_or_default()
    }
}

/// One entry of a menu as read from the app, before filtering.
#[derive(Debug, Clone, Default)]
pub struct RawItem {
    /// Empty for separators.
    pub title: String,
    pub enabled: bool,
    pub has_shortcut: bool,
    /// Items of its submenu, if it opens one. Empty when it wasn't read.
    pub submenu: Option<Vec<RawItem>>,
}

/// Submenus that list the user's recent files, services or favorites rather
/// than commands. Neither read nor offered.
const SKIPPED_SUBMENUS: &[&str] = &[
    "open recent",
    "recent items",
    "recently closed",
    "recently visited",
    "services",
    "favorites",
];

/// Top-level menus that end in a list of the user's pages, bookmarks or
/// windows. Only their fixed commands are offered.
const LIST_MENUS: &[&str] = &["history", "bookmarks", "window"];

/// Items that end the session or destroy data. Commands run without asking,
/// so these are never offered.
const DENIED_PREFIXES: &[&str] = &[
    "quit",
    "force quit",
    "log out",
    "sign out",
    "shut down",
    "restart",
    "erase",
    "empty",
    "revert",
    "discard",
    "move to trash",
    "delete",
    "remove",
];

fn normalized(title: &str) -> String {
    title
        .trim()
        .trim_end_matches('…')
        .trim_end_matches("...")
        .trim()
        .to_lowercase()
}

pub fn skip_submenu(title: &str) -> bool {
    let title = normalized(title);
    // "Open Recent", "Recent Folders", "Recently Closed"...: whatever the
    // app calls it, a list of recent things is the user's own content.
    title.contains("recent") || SKIPPED_SUBMENUS.contains(&title.as_str())
}

pub fn is_list_menu(title: &str) -> bool {
    LIST_MENUS.contains(&normalized(title).as_str())
}

fn denied(title: &str) -> bool {
    let title = normalized(title);
    DENIED_PREFIXES.iter().any(|prefix| {
        title
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    })
}

fn opens_dialog(title: &str) -> bool {
    let title = title.trim_end();
    title.ends_with('…') || title.ends_with("...")
}

/// The commands worth offering from `menus`, the menu bar's top-level menus
/// in order, the first being the Apple menu.
pub fn commands(menus: &[(String, Vec<RawItem>)]) -> Vec<MenuItem> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (title, items) in menus.iter().skip(1) {
        if title.trim().is_empty() || denied(title) {
            continue;
        }
        let path = vec![title.trim().to_string()];
        collect(items, &path, is_list_menu(title), &mut out, &mut seen);
    }
    out
}

fn collect(
    items: &[RawItem],
    path: &[String],
    list_menu: bool,
    out: &mut Vec<MenuItem>,
    seen: &mut HashSet<String>,
) {
    let mut section = 0;
    for item in items {
        let title = item.title.trim();
        if title.is_empty() {
            section += 1;
            continue;
        }
        if !item.enabled || denied(title) {
            continue;
        }
        let mut item_path = path.to_vec();
        item_path.push(title.to_string());
        match &item.submenu {
            Some(children) => {
                if !skip_submenu(title) && !list_menu {
                    collect(children, &item_path, false, out, seen);
                }
            }
            None => {
                if list_menu && section > 0 && !item.has_shortcut && !opens_dialog(title) {
                    continue;
                }
                let command = MenuItem { path: item_path };
                if seen.insert(command.label()) {
                    out.push(command);
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub use platform::{capture, press};

#[cfg(not(target_os = "macos"))]
pub fn capture(_pid: i32) -> Vec<MenuItem> {
    Vec::new()
}

#[cfg(not(target_os = "macos"))]
pub fn press(_pid: i32, _path: &[String]) -> Result<(), String> {
    Err("menu commands are only available on macOS".to_string())
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{commands, is_list_menu, skip_submenu, MenuItem, RawItem};
    use crate::voice_control::ax::{Element, AX_API_DISABLED, AX_CANNOT_COMPLETE};
    use log::debug;
    use std::time::{Duration, Instant};

    /// Submenu depth read below each top-level menu ("Mailbox > Go To >
    /// Inbox" is depth 1).
    const MAX_DEPTH: usize = 2;
    /// How long one Accessibility request may take before the app is
    /// treated as unresponsive (the system default is 6 s).
    const CAPTURE_TIMEOUT_SECS: f32 = 0.25;
    const PRESS_TIMEOUT_SECS: f32 = 1.0;
    /// Menus are read while transcription runs; stop reading past this.
    const CAPTURE_BUDGET: Duration = Duration::from_millis(1200);
    const MAX_ITEMS_READ: usize = 1500;

    fn menu_bar(pid: i32, timeout: f32) -> Result<Element, String> {
        let app = Element::application(pid, timeout).ok_or("could not reach the app")?;
        match app.element("AXMenuBar") {
            Ok(Some(bar)) => Ok(bar),
            Ok(None) => Err("the app has no menu bar".to_string()),
            Err(AX_API_DISABLED) => {
                Err("Handy needs the Accessibility permission to use app menus".to_string())
            }
            Err(e) => Err(format!("could not read the app's menus (AX error {e})")),
        }
    }

    struct Budget {
        deadline: Instant,
        items_left: usize,
    }

    impl Budget {
        fn spend(&mut self) -> bool {
            if self.items_left == 0 || Instant::now() >= self.deadline {
                return false;
            }
            self.items_left -= 1;
            true
        }
    }

    fn read_menu(
        menu: &Element,
        depth: usize,
        list_menu: bool,
        budget: &mut Budget,
    ) -> Vec<RawItem> {
        let mut items = Vec::new();
        for element in menu.children() {
            if !budget.spend() {
                break;
            }
            let title = element.title();
            if title.trim().is_empty() {
                // A separator.
                items.push(RawItem::default());
                continue;
            }
            let enabled = element.flag("AXEnabled").unwrap_or(false);
            // Shortcuts only decide which items of a list menu are commands.
            let has_shortcut = list_menu
                && element
                    .string("AXMenuItemCmdChar")
                    .is_some_and(|key| !key.trim().is_empty());
            let submenu = element.children().into_iter().next().map(|submenu| {
                if enabled && depth < MAX_DEPTH && !list_menu && !skip_submenu(&title) {
                    read_menu(&submenu, depth + 1, false, budget)
                } else {
                    Vec::new()
                }
            });
            items.push(RawItem {
                title,
                enabled,
                has_shortcut,
                submenu,
            });
        }
        items
    }

    /// The menu commands of the app with process id `pid`. Empty if the app
    /// can't be read (no Accessibility permission, unresponsive, no menus).
    pub fn capture(pid: i32) -> Vec<MenuItem> {
        let started = Instant::now();
        let bar = match menu_bar(pid, CAPTURE_TIMEOUT_SECS) {
            Ok(bar) => bar,
            Err(e) => {
                debug!("No menu commands: {e}");
                return Vec::new();
            }
        };
        let mut budget = Budget {
            deadline: started + CAPTURE_BUDGET,
            items_left: MAX_ITEMS_READ,
        };
        let menus: Vec<(String, Vec<RawItem>)> = bar
            .children()
            .into_iter()
            .enumerate()
            .map(|(index, top)| {
                let title = top.title();
                // The Apple menu (first) is never offered, so it isn't read.
                let items = match top.children().into_iter().next() {
                    Some(menu) if index > 0 => {
                        read_menu(&menu, 0, is_list_menu(&title), &mut budget)
                    }
                    _ => Vec::new(),
                };
                (title, items)
            })
            .collect();
        let found = commands(&menus);
        debug!(
            "Read {} menu commands in {} ms",
            found.len(),
            started.elapsed().as_millis()
        );
        found
    }

    /// Whether a menu element is the one titled `title` in a command's path,
    /// which holds titles trimmed.
    fn titled(element: &Element, title: &str) -> bool {
        element.title().trim() == title
    }

    /// Press the menu item at `path` in the app with process id `pid`.
    pub fn press(pid: i32, path: &[String]) -> Result<(), String> {
        let bar = menu_bar(pid, PRESS_TIMEOUT_SECS)?;
        let (top_title, rest) = path.split_first().ok_or("empty menu path")?;
        let top = bar
            .children()
            .into_iter()
            .skip(1)
            .find(|element| titled(element, top_title))
            .ok_or_else(|| format!("the '{top_title}' menu is gone"))?;
        let mut menu = top
            .children()
            .into_iter()
            .next()
            .ok_or_else(|| format!("the '{top_title}' menu is empty"))?;
        for (index, title) in rest.iter().enumerate() {
            let item = menu
                .children()
                .into_iter()
                .find(|element| titled(element, title))
                .ok_or_else(|| format!("'{title}' is no longer in the menu"))?;
            if index + 1 == rest.len() {
                return match item.press() {
                    Ok(()) => Ok(()),
                    // The app took the press but hasn't answered: an item that
                    // opens a dialog blocks until the dialog closes.
                    Err(AX_CANNOT_COMPLETE) => {
                        debug!("'{title}' is still running, probably showing a dialog");
                        Ok(())
                    }
                    Err(e) => Err(format!("macOS didn't run '{title}' (AX error {e})")),
                };
            }
            menu = item
                .children()
                .into_iter()
                .next()
                .ok_or_else(|| format!("'{title}' has no submenu"))?;
        }
        Err("empty menu path".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(title: &str) -> RawItem {
        RawItem {
            title: title.to_string(),
            enabled: true,
            has_shortcut: false,
            submenu: None,
        }
    }

    fn shortcut(title: &str) -> RawItem {
        RawItem {
            has_shortcut: true,
            ..leaf(title)
        }
    }

    fn disabled(title: &str) -> RawItem {
        RawItem {
            enabled: false,
            ..leaf(title)
        }
    }

    fn separator() -> RawItem {
        RawItem::default()
    }

    fn submenu(title: &str, items: Vec<RawItem>) -> RawItem {
        RawItem {
            submenu: Some(items),
            ..leaf(title)
        }
    }

    fn menu(title: &str, items: Vec<RawItem>) -> (String, Vec<RawItem>) {
        (title.to_string(), items)
    }

    fn labels(menus: &[(String, Vec<RawItem>)]) -> Vec<String> {
        commands(menus).iter().map(MenuItem::label).collect()
    }

    #[test]
    fn skips_the_apple_menu_and_session_enders() {
        let found = labels(&[
            menu("Apple", vec![leaf("About This Mac"), leaf("Restart…")]),
            menu(
                "Safari",
                vec![
                    leaf("Settings…"),
                    separator(),
                    leaf("Quit Safari"),
                    leaf("Quit and Keep Windows"),
                ],
            ),
            menu(
                "File",
                vec![
                    leaf("New Window"),
                    leaf("Revert To Saved"),
                    leaf("Empty Trash…"),
                    leaf("Restartable Task"),
                ],
            ),
        ]);
        assert_eq!(
            found,
            vec![
                "Safari > Settings…",
                "File > New Window",
                "File > Restartable Task"
            ]
        );
    }

    #[test]
    fn walks_submenus_but_not_recent_files_or_services() {
        let found = labels(&[
            menu("Apple", vec![]),
            menu(
                "Mail",
                vec![
                    submenu("Services", vec![leaf("Make Sticky")]),
                    leaf("Settings…"),
                ],
            ),
            menu(
                "Mailbox",
                vec![
                    submenu("Go To", vec![leaf("Inbox"), leaf("Sent")]),
                    leaf("Get All New Mail"),
                ],
            ),
            menu(
                "File",
                vec![submenu("Open Recent", vec![leaf("Tax return 2025.pdf")])],
            ),
        ]);
        assert_eq!(
            found,
            vec![
                "Mail > Settings…",
                "Mailbox > Go To > Inbox",
                "Mailbox > Go To > Sent",
                "Mailbox > Get All New Mail",
            ]
        );
    }

    #[test]
    fn keeps_only_fixed_commands_of_list_menus() {
        let found = labels(&[
            menu("Apple", vec![]),
            menu(
                "History",
                vec![
                    shortcut("Back"),
                    leaf("Home"),
                    separator(),
                    leaf("My bank - Account summary"),
                    submenu("Earlier Today", vec![leaf("Some private page")]),
                    separator(),
                    shortcut("Show All History"),
                    leaf("Clear History…"),
                ],
            ),
            menu(
                "Window",
                vec![
                    shortcut("Minimize"),
                    leaf("Zoom"),
                    separator(),
                    leaf("Inbox — 1,234 messages"),
                ],
            ),
        ]);
        assert_eq!(
            found,
            vec![
                "History > Back",
                "History > Home",
                "History > Show All History",
                "History > Clear History…",
                "Window > Minimize",
                "Window > Zoom",
            ]
        );
    }

    #[test]
    fn drops_disabled_items_and_duplicates() {
        let found = labels(&[
            menu("Apple", vec![]),
            menu(
                "Edit",
                vec![disabled("Undo"), leaf("Copy"), leaf("Copy"), separator()],
            ),
        ]);
        assert_eq!(found, vec!["Edit > Copy"]);
    }

    #[test]
    fn any_recent_list_is_skipped() {
        let found = labels(&[
            menu("Apple", vec![]),
            menu(
                "Go",
                vec![
                    leaf("Home"),
                    RawItem {
                        title: "Recent Folders".into(),
                        enabled: true,
                        has_shortcut: false,
                        submenu: Some(vec![leaf("Taxes"), leaf("Passwords")]),
                    },
                ],
            ),
        ]);
        assert_eq!(found, vec!["Go > Home"]);
    }

    #[test]
    fn items_that_delete_are_never_offered() {
        let found = labels(&[
            menu("Apple", vec![]),
            menu(
                "File",
                vec![
                    leaf("New Window"),
                    leaf("Move to Trash"),
                    leaf("Delete Conversation…"),
                    leaf("Remove from Favorites"),
                ],
            ),
        ]);
        assert_eq!(found, vec!["File > New Window"]);
    }

    #[test]
    fn titles_are_trimmed_to_match_when_pressed() {
        let found = labels(&[
            menu("Apple", vec![]),
            menu(" View ", vec![leaf(" Show Downloads ")]),
        ]);
        assert_eq!(found, vec!["View > Show Downloads"]);
    }

    #[test]
    fn labels_and_titles() {
        let item = MenuItem {
            path: vec!["Mailbox".into(), "Go To".into(), "Inbox".into()],
        };
        assert_eq!(item.label(), "Mailbox > Go To > Inbox");
        assert_eq!(item.title(), "Inbox");
    }
}
