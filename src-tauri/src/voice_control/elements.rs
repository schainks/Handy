//! What's on screen in the front window that can be clicked: links, buttons,
//! tabs, checkboxes and text fields, each named by its own text, such as
//! "Octopus (link)" or "Search Wikipedia (field)".
//!
//! Items are read only when the user asks to click something, so the text on
//! screen is sent to Jev only then, never with ordinary dictation. Clicking
//! goes through the Accessibility API (press a link, focus a field), so the
//! mouse pointer doesn't move. "Click this" clicks where the pointer already
//! is.

// The helpers below are used by the macOS reader and by tests everywhere.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use super::candidates::ngrams;

/// Longest item name offered, in characters.
const MAX_NAME_CHARS: usize = 80;

/// One item that can be clicked.
pub struct Clickable {
    /// What Jev reads and picks, e.g. "Octopus (link)".
    pub label: String,
    /// The item's own text, for the overlay.
    pub name: String,
    is_field: bool,
    #[cfg(target_os = "macos")]
    element: super::ax::Element,
}

/// How Jev reads an Accessibility role, or None for roles that aren't
/// clicked (text, images, groups...).
pub fn kind(role: &str, subrole: &str) -> Option<&'static str> {
    Some(match role {
        "AXLink" => "link",
        "AXButton" | "AXDisclosureTriangle" => "button",
        "AXMenuButton" | "AXPopUpButton" => "menu",
        "AXCheckBox" if subrole == "AXSwitch" => "switch",
        "AXCheckBox" => "checkbox",
        "AXRadioButton" if subrole == "AXTabButton" => "tab",
        "AXRadioButton" => "option",
        "AXTextField" | "AXTextArea" | "AXComboBox" => "field",
        _ => return None,
    })
}

/// What to call a text field that has no label or placeholder, so that "click
/// the search box" or "click the password field" can still find it.
pub fn unnamed_field(subrole: &str) -> &'static str {
    match subrole {
        "AXSearchField" => "search",
        "AXSecureTextField" => "password",
        _ => "text",
    }
}

/// One line, trimmed, at most `MAX_NAME_CHARS` characters.
pub fn clean_name(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(MAX_NAME_CHARS) {
        Some((cut, _)) => format!("{}…", one_line[..cut].trim_end()),
        None => one_line,
    }
}

/// Labels for items in screen order: "name (kind)", with repeats numbered
/// ("edit (link)", "edit (link) 2") so every label picks exactly one item.
pub fn labels(items: &[(String, &str)]) -> Vec<String> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    items
        .iter()
        .map(|(name, kind)| {
            let label = format!("{name} ({kind})");
            let count = seen.entry(label.to_lowercase()).or_insert(0);
            *count += 1;
            if *count == 1 {
                label
            } else {
                format!("{label} {count}")
            }
        })
        .collect()
}

/// Indexes of the items to offer: all of them when they fit, otherwise those
/// whose names look most like something in the utterance, in screen order.
pub fn shortlist(names: &[String], utterance: &str, cap: usize) -> Vec<usize> {
    if names.len() <= cap {
        return (0..names.len()).collect();
    }
    let grams = ngrams(utterance);
    let mut scored: Vec<(f64, usize)> = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let name = name.to_lowercase();
            let likeness = grams
                .iter()
                .map(|gram| strsim::jaro_winkler(&name, gram))
                .fold(0.0, f64::max);
            (likeness, index)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut kept: Vec<usize> = scored.into_iter().take(cap).map(|(_, i)| i).collect();
    kept.sort_unstable();
    kept
}

#[cfg(target_os = "macos")]
pub use platform::{click_pointer, press, read};

#[cfg(not(target_os = "macos"))]
pub fn read(_pid: i32) -> Result<Vec<Clickable>, String> {
    Err("clicking is only available on macOS".to_string())
}

#[cfg(not(target_os = "macos"))]
pub fn press(_item: &Clickable) -> Result<(), String> {
    Err("clicking is only available on macOS".to_string())
}

#[cfg(not(target_os = "macos"))]
pub fn click_pointer() -> Result<String, String> {
    Err("clicking is only available on macOS".to_string())
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{clean_name, kind, labels, unnamed_field, Clickable};
    use crate::voice_control::ax::{self, Element, AX_API_DISABLED, AX_CANNOT_COMPLETE};
    use log::debug;
    use objc2_core_foundation::{CGPoint, CGSize};
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    const TIMEOUT_SECS: f32 = 0.5;
    /// Reading happens after the user asked to click, so it holds up the
    /// click: stop reading past this.
    const BUDGET: Duration = Duration::from_millis(1500);
    /// Window controls (toolbar, tab bar, sidebars) around the page.
    const MAX_WINDOW_NODES: usize = 600;
    const MAX_WINDOW_DEPTH: usize = 12;
    /// Visible items asked from each web view.
    const MAX_PAGE_ITEMS: i32 = 400;
    /// Nodes walked in a web view that can't be searched.
    const MAX_PAGE_NODES: usize = 3000;
    const MAX_WEB_AREAS: usize = 3;
    /// Containers of rows and cells (message lists, tables) hold thousands
    /// of items; they aren't walked.
    const SKIPPED_CONTAINERS: &[&str] = &["AXTable", "AXOutline", "AXBrowser", "AXList", "AXGrid"];

    /// The item's name: its title or description, the label naming it, a
    /// field's placeholder (never what's typed in it), the text inside it, or
    /// its value.
    fn name_of(element: &Element, kind: &str) -> String {
        if let Some(name) = own_name(element).or_else(|| label_of(element)) {
            return name;
        }
        if kind == "field" {
            return element
                .string("AXPlaceholderValue")
                .map(|text| clean_name(&text))
                .unwrap_or_default();
        }
        // A link's words are usually text elements inside it; its own value
        // can be its address.
        let words = static_text(element);
        if !words.is_empty() {
            return words;
        }
        element
            .string("AXValue")
            .map(|text| clean_name(&text))
            .unwrap_or_default()
    }

    /// The element's title, or else its description.
    fn own_name(element: &Element) -> Option<String> {
        ["AXTitle", "AXDescription"]
            .into_iter()
            .find_map(|attribute| {
                element
                    .string(attribute)
                    .map(|text| clean_name(&text))
                    .filter(|text| !text.is_empty())
            })
    }

    /// The text of the HTML <label> naming a form control. WebKit points to
    /// the label rather than repeating it in the control's title. Only the
    /// label's own text is read: a label can wrap its field, whose value
    /// must not be.
    fn label_of(element: &Element) -> Option<String> {
        let label = element.element("AXTitleUIElement").ok()??;
        let text = if label.role() == "AXStaticText" {
            label
                .string("AXValue")
                .map(|text| clean_name(&text))
                .unwrap_or_default()
        } else {
            static_text(&label)
        };
        (!text.is_empty()).then_some(text)
    }

    /// The words of the first text elements directly inside `element`.
    fn static_text(element: &Element) -> String {
        let words: Vec<String> = element
            .children()
            .into_iter()
            .take(4)
            .filter(|child| child.role() == "AXStaticText")
            .filter_map(|child| child.string("AXValue"))
            .collect();
        clean_name(&words.join(" "))
    }

    /// The item's kind and name, if it's something to click. Items other
    /// than fields need a name to be picked by.
    fn describe(element: &Element) -> Option<(&'static str, String)> {
        let role = element.role();
        let subrole = element.string("AXSubrole").unwrap_or_default();
        let kind = kind(&role, &subrole)?;
        let name = name_of(element, kind);
        if !name.is_empty() {
            Some((kind, name))
        } else if kind == "field" {
            Some((kind, unnamed_field(&subrole).to_string()))
        } else {
            None
        }
    }

    fn visible_in(frame: Option<(CGPoint, CGSize)>, window: (CGPoint, CGSize)) -> bool {
        let Some((origin, size)) = frame else {
            return false;
        };
        let (window_origin, window_size) = window;
        size.width > 0.0
            && size.height > 0.0
            && origin.x < window_origin.x + window_size.width
            && origin.x + size.width > window_origin.x
            && origin.y < window_origin.y + window_size.height
            && origin.y + size.height > window_origin.y
    }

    struct Found {
        element: Element,
        kind: &'static str,
        name: String,
    }

    /// Walk a web view that doesn't answer the visible-items search,
    /// keeping items inside the window.
    fn walk_page(
        page: Element,
        window: Option<(CGPoint, CGSize)>,
        started: Instant,
        found: &mut Vec<Found>,
    ) {
        let mut queue = VecDeque::from([page]);
        let mut visited = 0;
        while let Some(element) = queue.pop_front() {
            if visited >= MAX_PAGE_NODES || started.elapsed() > BUDGET {
                break;
            }
            visited += 1;
            if let Some((kind, name)) = describe(&element) {
                if window.is_none_or(|window| visible_in(element.frame(), window)) {
                    found.push(Found {
                        element,
                        kind,
                        name,
                    });
                }
                continue;
            }
            queue.extend(element.children());
        }
    }

    /// The clickable items in the front window of the app with process id
    /// `pid`: its controls, then the visible items of any page it shows.
    pub fn read(pid: i32) -> Result<Vec<Clickable>, String> {
        let started = Instant::now();
        let app = Element::application(pid, TIMEOUT_SECS).ok_or("could not reach the app")?;
        // Chromium and Electron apps (Chrome, Arc, Slack) build their page's
        // Accessibility tree only once an app asks for it; others ignore this.
        let _ = app.set_flag("AXManualAccessibility", true);
        let window = match app.element("AXFocusedWindow") {
            Ok(Some(window)) => window,
            Ok(None) => app
                .elements("AXWindows")
                .into_iter()
                .next()
                .ok_or("the app has no open window")?,
            Err(AX_API_DISABLED) => {
                return Err("Handy needs the Accessibility permission to click things".into())
            }
            Err(e) => return Err(format!("could not read the front window (AX error {e})")),
        };
        let window_frame = window.frame();

        let mut found = Vec::new();
        let mut pages = Vec::new();
        let mut queue = VecDeque::from([(window, 0)]);
        let mut visited = 0;
        while let Some((element, depth)) = queue.pop_front() {
            if visited >= MAX_WINDOW_NODES || started.elapsed() > BUDGET {
                break;
            }
            visited += 1;
            let role = element.role();
            if role == "AXWebArea" {
                pages.push(element);
                continue;
            }
            if SKIPPED_CONTAINERS.contains(&role.as_str()) {
                continue;
            }
            if let Some((kind, name)) = describe(&element) {
                found.push(Found {
                    element,
                    kind,
                    name,
                });
                continue;
            }
            if depth < MAX_WINDOW_DEPTH {
                queue.extend(
                    element
                        .children()
                        .into_iter()
                        .map(|child| (child, depth + 1)),
                );
            }
        }

        for page in pages.into_iter().take(MAX_WEB_AREAS) {
            match page.search_visible(&["AXLinkSearchKey", "AXControlSearchKey"], MAX_PAGE_ITEMS) {
                Ok(items) => {
                    for element in items {
                        if started.elapsed() > BUDGET {
                            break;
                        }
                        if let Some((kind, name)) = describe(&element) {
                            found.push(Found {
                                element,
                                kind,
                                name,
                            });
                        }
                    }
                }
                Err(e) => {
                    debug!("Page can't be searched (AX error {e}); walking it instead");
                    walk_page(page, window_frame, started, &mut found);
                }
            }
        }

        let names: Vec<(String, &str)> = found
            .iter()
            .map(|item| (item.name.clone(), item.kind))
            .collect();
        let items: Vec<Clickable> = labels(&names)
            .into_iter()
            .zip(found)
            .map(|(label, item)| Clickable {
                label,
                name: item.name,
                is_field: item.kind == "field",
                element: item.element,
            })
            .collect();
        debug!(
            "Read {} clickable items in {} ms",
            items.len(),
            started.elapsed().as_millis()
        );
        Ok(items)
    }

    /// Press a link or button, or put the cursor in a field.
    pub fn press(item: &Clickable) -> Result<(), String> {
        let result = if item.is_field {
            item.element.focus()
        } else {
            item.element.press()
        };
        match result {
            Ok(()) => Ok(()),
            // Taken, but the app hasn't answered: probably a dialog it opened.
            Err(AX_CANNOT_COMPLETE) => Ok(()),
            Err(e) => Err(format!("macOS didn't click '{}' (AX error {e})", item.name)),
        }
    }

    /// Click where the pointer is, without moving it. Returns the name of
    /// what was clicked, when it has one.
    pub fn click_pointer() -> Result<String, String> {
        let point = ax::pointer_location().ok_or("couldn't find the pointer")?;
        let mut name = String::new();
        if let Some(mut element) =
            Element::system_wide(TIMEOUT_SECS).and_then(|screen| screen.element_at(point))
        {
            // The deepest element is often the text inside a link or button.
            for _ in 0..4 {
                if let Some((_, found)) = describe(&element) {
                    name = found;
                    break;
                }
                match element.element("AXParent") {
                    Ok(Some(parent)) => element = parent,
                    _ => break,
                }
            }
        }
        ax::click_at(point)?;
        Ok(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_of_clickable_roles() {
        assert_eq!(kind("AXLink", ""), Some("link"));
        assert_eq!(kind("AXButton", ""), Some("button"));
        assert_eq!(kind("AXRadioButton", "AXTabButton"), Some("tab"));
        assert_eq!(kind("AXCheckBox", "AXSwitch"), Some("switch"));
        assert_eq!(kind("AXTextField", "AXSearchField"), Some("field"));
        assert_eq!(kind("AXStaticText", ""), None);
        assert_eq!(kind("AXGroup", ""), None);
    }

    #[test]
    fn unnamed_fields_are_named_by_kind() {
        assert_eq!(unnamed_field("AXSearchField"), "search");
        assert_eq!(unnamed_field("AXSecureTextField"), "password");
        assert_eq!(unnamed_field(""), "text");
    }

    #[test]
    fn names_are_one_short_line() {
        assert_eq!(
            clean_name("  Octopus\n  intelligence "),
            "Octopus intelligence"
        );
        let long = "word ".repeat(40);
        let cleaned = clean_name(&long);
        assert!(cleaned.chars().count() <= MAX_NAME_CHARS + 1);
        assert!(cleaned.ends_with('…'));
    }

    #[test]
    fn repeated_labels_are_numbered() {
        let items = vec![
            ("edit".to_string(), "link"),
            ("Octopus".to_string(), "link"),
            ("Edit".to_string(), "link"),
            ("edit".to_string(), "button"),
        ];
        assert_eq!(
            labels(&items),
            vec![
                "edit (link)",
                "Octopus (link)",
                "Edit (link) 2",
                "edit (button)"
            ]
        );
    }

    #[test]
    fn shortlist_keeps_the_named_item_in_screen_order() {
        let mut names: Vec<String> = (0..500).map(|i| format!("Footnote {i}")).collect();
        names.insert(250, "Cephalopod".into());
        names.push("Random article".into());
        let kept = shortlist(&names, "click cephalopod", 30);
        assert_eq!(kept.len(), 30);
        assert!(kept.contains(&250));
        assert!(kept.windows(2).all(|pair| pair[0] < pair[1]));

        assert_eq!(shortlist(&names[..3], "anything", 30), vec![0, 1, 2]);
    }
}
