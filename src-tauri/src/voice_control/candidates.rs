//! Code proposes, Jev chooses. Jev picks an action but doesn't write its
//! argument, so code offers the plausible arguments as options: app names from
//! the desktop, the frontmost app's menu commands, and spans cut from the
//! utterance at cue words ("search for", "type", "go to"...). Jev then picks
//! among them in the same request. Numbers are unambiguous enough to parse
//! outright.

use super::context::DesktopContext;
use super::jev::Profile;
use super::menus::MenuItem;
use super::registry::{Action, ArgKind};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;

/// One Choice over apps, well under Jev's 255-option limit.
pub const MAX_APP_CANDIDATES: usize = 200;
/// One Choice over menu commands. Most apps have fewer.
pub const MAX_MENU_CANDIDATES: usize = 200;
pub const MAX_SPANS: usize = 12;

/// A small local model handles about twenty options per question well and
/// loses the thread beyond that, so its requests carry shortlists.
pub const LOCAL_MAX_ACTIONS: usize = 14;
pub const LOCAL_MAX_APPS: usize = 15;
pub const LOCAL_MAX_MENUS: usize = 20;
pub const LOCAL_MAX_TARGETS: usize = 20;

#[derive(Debug, Clone, Default)]
pub struct Proposal {
    pub apps: Vec<String>,
    /// Labels of the frontmost app's menu commands ("View > Zoom In").
    pub menus: Vec<String>,
    pub spans: Vec<String>,
    pub number: Option<u32>,
}

pub fn propose(utterance: &str, ctx: &DesktopContext, profile: Profile) -> Proposal {
    let (max_apps, max_menus) = match profile {
        Profile::Jev => (MAX_APP_CANDIDATES, MAX_MENU_CANDIDATES),
        Profile::Local => (LOCAL_MAX_APPS, LOCAL_MAX_MENUS),
    };
    Proposal {
        apps: shortlist_apps(&ctx.installed_apps, &ctx.running_apps, utterance, max_apps),
        menus: shortlist_menus(&ctx.menu_items, utterance, max_menus),
        spans: text_spans(utterance),
        number: parse_number(utterance),
    }
}

/// Words after which the argument usually starts. Longer cues come first so
/// "search for" wins over "for" when both would cut the same place.
const SPAN_CUES: &[&str] = &[
    "search the web for",
    "search for",
    "look up",
    "google",
    "search",
    "find",
    "go to",
    "visit",
    "open",
    "type",
    "write",
    "say",
    "for",
    "about",
    "to",
    "called",
    "named",
];

static CUE_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    SPAN_CUES
        .iter()
        .map(|cue| Regex::new(&format!(r"(?i)\b{}\b", regex::escape(cue))).unwrap())
        .collect()
});

static QUOTED: Lazy<Regex> = Lazy::new(|| Regex::new(r#"["“”]([^"“”]+)["“”]"#).unwrap());

fn trim_span(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ".,!?;:\"“”'".contains(c))
}

fn push_span(spans: &mut Vec<String>, candidate: &str) {
    let candidate = trim_span(candidate);
    if candidate.is_empty()
        || spans
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(candidate))
    {
        return;
    }
    spans.push(candidate.to_string());
}

/// The whole utterance first, then quoted text, then the remainder after every
/// cue word, then everything after the first word (the imperative verb).
pub fn text_spans(utterance: &str) -> Vec<String> {
    let base = trim_span(utterance);
    let mut spans = Vec::new();
    push_span(&mut spans, base);

    for quoted in QUOTED.captures_iter(base) {
        push_span(&mut spans, &quoted[1]);
    }
    for cue in CUE_PATTERNS.iter() {
        for found in cue.find_iter(base) {
            push_span(&mut spans, &base[found.end()..]);
        }
    }
    if let Some((_, rest)) = base.split_once(char::is_whitespace) {
        push_span(&mut spans, rest);
    }

    spans.truncate(MAX_SPANS);
    spans
}

fn unit(word: &str) -> Option<u32> {
    let value = match word {
        "zero" => 0,
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        "eleven" => 11,
        "twelve" => 12,
        "thirteen" => 13,
        "fourteen" => 14,
        "fifteen" => 15,
        "sixteen" => 16,
        "seventeen" => 17,
        "eighteen" => 18,
        "nineteen" => 19,
        _ => return None,
    };
    Some(value)
}

fn tens(word: &str) -> Option<u32> {
    let value = match word {
        "twenty" => 20,
        "thirty" => 30,
        "forty" => 40,
        "fifty" => 50,
        "sixty" => 60,
        "seventy" => 70,
        "eighty" => 80,
        "ninety" => 90,
        _ => return None,
    };
    Some(value)
}

static DIGITS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(\d{1,4})\b").unwrap());

/// The first number said, in digits ("30%") or words ("thirty", "twenty-five",
/// "a hundred", "half", "max").
pub fn parse_number(utterance: &str) -> Option<u32> {
    if let Some(found) = DIGITS.captures(utterance) {
        return found[1].parse().ok();
    }

    let lower = utterance.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    for (i, word) in words.iter().enumerate() {
        let next = words.get(i + 1).copied();
        match *word {
            "half" => return Some(50),
            "max" | "maximum" | "hundred" => return Some(100),
            _ => {}
        }
        if let Some(ten) = tens(word) {
            let ones = next.and_then(unit).filter(|u| (1..10).contains(u));
            return Some(ten + ones.unwrap_or(0));
        }
        if let Some(value) = unit(word) {
            return Some(if next == Some("hundred") {
                value * 100
            } else {
                value
            });
        }
    }
    None
}

/// Utterance n-grams (1 to 3 words) to compare app names against.
pub(super) fn ngrams(utterance: &str) -> Vec<String> {
    let words: Vec<String> = utterance
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '.')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let mut grams = Vec::new();
    for n in 1..=3 {
        for window in words.windows(n) {
            grams.push(window.join(" "));
        }
    }
    grams
}

/// Running apps first, then installed ones, deduplicated. When there are more
/// than one Choice should carry, keep those whose names look most like
/// something in the utterance (running apps get a head start).
pub fn shortlist_apps(
    installed: &[String],
    running: &[String],
    utterance: &str,
    cap: usize,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut apps: Vec<(String, bool)> = Vec::new();
    for (name, is_running) in running
        .iter()
        .map(|n| (n, true))
        .chain(installed.iter().map(|n| (n, false)))
    {
        let name = name.trim();
        if !name.is_empty() && name != "none" && seen.insert(name.to_lowercase()) {
            apps.push((name.to_string(), is_running));
        }
    }
    if apps.len() <= cap {
        return apps.into_iter().map(|(name, _)| name).collect();
    }

    let grams = ngrams(utterance);
    let mut scored: Vec<(f64, usize, String)> = apps
        .into_iter()
        .enumerate()
        .map(|(order, (name, is_running))| {
            let lower = name.to_lowercase();
            let likeness = grams
                .iter()
                .map(|gram| strsim::jaro_winkler(&lower, gram))
                .fold(0.0, f64::max);
            (likeness + if is_running { 0.2 } else { 0.0 }, order, name)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(cap);
    scored.into_iter().map(|(_, _, name)| name).collect()
}

/// Letters and digits only, lowercase: "Text Edit", "text-edit" and "TextEdit" match.
fn compact(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Verb phrases that ask for an app, and the action each one means. Longest first.
const APP_VERBS: &[(&[&str], &str)] = &[
    (&["take", "me", "to"], "open_app"),
    (&["switch", "over", "to"], "open_app"),
    (&["jump", "over", "to"], "open_app"),
    (&["switch", "to"], "open_app"),
    (&["bring", "up"], "open_app"),
    (&["pull", "up"], "open_app"),
    (&["fire", "up"], "open_app"),
    (&["open", "up"], "open_app"),
    (&["go", "to"], "open_app"),
    (&["open"], "open_app"),
    (&["launch"], "open_app"),
    (&["start"], "open_app"),
    (&["run"], "open_app"),
    (&["activate"], "open_app"),
    (&["quit"], "quit_app"),
    (&["exit"], "quit_app"),
    (&["terminate"], "quit_app"),
    (&["kill"], "quit_app"),
    (&["hide"], "hide_app"),
];
const LEADING_FILLER: &[&str] = &[
    "please", "can", "could", "you", "hey", "okay", "ok", "so", "just", "quickly",
];
const TRAILING_FILLER: &[&str] = &["please", "now", "thanks", "for", "me"];

/// "Open Safari", "quit text edit", "hide Messages": an app named exactly,
/// spaces and case aside, needs no model. It can't know which apps are on this
/// Mac, and speech-to-text splits names like TextEdit into two words.
/// Returns the action and the app's own name.
pub fn named_app_command(utterance: &str, apps: &[String]) -> Option<(&'static str, String)> {
    let lower = utterance.to_lowercase();
    let mut words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '.')
        .map(|word| word.trim_matches('.'))
        .filter(|word| !word.is_empty())
        .collect();
    while words
        .first()
        .is_some_and(|word| LEADING_FILLER.contains(word))
    {
        words.remove(0);
    }
    if words.starts_with(&["go", "ahead", "and"]) {
        words.drain(..3);
    }
    let (verb, action) = APP_VERBS.iter().find(|(verb, _)| words.starts_with(verb))?;
    let mut rest = &words[verb.len()..];
    if rest.first() == Some(&"the") {
        rest = &rest[1..];
    }
    while rest
        .last()
        .is_some_and(|word| TRAILING_FILLER.contains(word))
    {
        rest = &rest[..rest.len() - 1];
    }
    if matches!(rest.last(), Some(&"app") | Some(&"application")) {
        rest = &rest[..rest.len() - 1];
    }
    if rest.is_empty() || rest.len() > 4 {
        return None;
    }
    let spoken = compact(&rest.concat());
    apps.iter()
        .find(|app| !spoken.is_empty() && compact(app) == spoken)
        .map(|app| (*action, app.clone()))
}

/// Words that fit any request and so say nothing about which action is meant.
/// Left out are "up", "out", "over" and "on", which do (turn it up, go back,
/// page up).
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "my", "me", "i", "it", "its", "this", "that", "these", "those", "to", "of",
    "in", "at", "for", "from", "with", "and", "or", "is", "are", "be", "as", "into", "please",
    "can", "could", "would", "you", "your", "we", "our", "some", "any", "one",
];

/// Words people use to ask for an action, for the shortlist only (the model
/// never sees them). Without them an action whose description says little about
/// how it's asked for ("go to github.com", "search for flights", "click Save")
/// loses to whatever the argument's words happen to resemble.
fn cues(action_id: &str) -> &'static str {
    match action_id {
        "open_app" => "launch start run switch bring up app application",
        "quit_app" => "quit exit close kill terminate stop end app application",
        "hide_app" => "hide tuck away app application",
        "open_website" => "go to visit navigate browse head take website site page url address load open",
        "web_search" => "search google look up find online web query",
        "click_element" => "click press tap select choose push hit check tick toggle button link tab checkbox field box menu dropdown option switch",
        "type_text" => "type write say enter words text",
        "undo" => "undo scratch revert take back",
        "redo" => "redo again reapply put back",
        "paste" => "paste clipboard drop",
        "window_left" => "window left half tile snap side",
        "window_right" => "window right half tile snap side",
        "window_maximize" => "window maximize fill screen big bigger enlarge expand",
        "window_center" => "window center centre middle recenter",
        "window_next_screen" => "window screen display monitor other second next move send",
        "window_minimize" => "window minimize dock shrink",
        "window_fullscreen" => "full screen fullscreen",
        "sleep_display" => "sleep display screen monitor off blank",
        "set_volume" => "volume level percent set sound",
        "volume_up" => "louder up raise increase crank sound volume",
        "volume_down" => "quieter softer down lower decrease sound volume",
        "lock_screen" => "lock secure screen computer",
        "screenshot" => "screenshot capture screen record",
        _ => "",
    }
}

/// How alike two stems must be to count as the same word.
const MIN_WORD_SIMILARITY: f64 = 0.85;

/// Word stems without stopwords, to match "louder" with "loud" and "tabs"
/// with "tab".
fn stems(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty() && !STOPWORDS.contains(word))
        .map(|word| {
            ["ing", "es", "s", "ed"]
                .iter()
                .find_map(|suffix| word.strip_suffix(suffix).filter(|stem| stem.len() > 2))
                .unwrap_or(word)
                .to_string()
        })
        .collect()
}

/// The `cap` actions most likely meant by `utterance`, best first, for a model
/// that can't weigh dozens of options. An action scores by how closely its
/// title and description share words with the utterance. The menu command
/// action also counts the words of the menu commands on offer, since its own
/// description can't say what the front app's menus hold.
pub fn shortlist_actions<'a>(
    actions: &'a [Action],
    utterance: &str,
    menu_labels: &[String],
    cap: usize,
) -> Vec<&'a Action> {
    if actions.len() <= cap {
        return actions.iter().collect();
    }
    let query = stems(utterance);
    let mut scored: Vec<(f64, usize)> = actions
        .iter()
        .enumerate()
        .map(|(order, action)| {
            let mut text = format!("{} {} {}", action.title, action.what, cues(&action.id));
            if action.arg == ArgKind::Menu {
                text.push(' ');
                text.push_str(&menu_labels.join(" "));
            }
            let words = stems(&text);
            let score = query
                .iter()
                .map(|word| {
                    words
                        .iter()
                        .map(|other| strsim::jaro_winkler(word, other))
                        .fold(0.0, f64::max)
                })
                .filter(|&similarity| similarity >= MIN_WORD_SIMILARITY)
                .map(|similarity| similarity.powi(4))
                .sum();
            (score, order)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(cap)
        .map(|(_, order)| &actions[order])
        .collect()
}

/// Every menu command when they fit in one Choice. Otherwise those whose
/// titles look most like something in the utterance, kept in menu order.
pub fn shortlist_menus(items: &[MenuItem], utterance: &str, cap: usize) -> Vec<String> {
    if items.len() <= cap {
        return items.iter().map(MenuItem::label).collect();
    }
    let grams = ngrams(utterance);
    let mut scored: Vec<(f64, usize)> = items
        .iter()
        .enumerate()
        .map(|(order, item)| {
            let title = item.title().to_lowercase();
            let likeness = grams
                .iter()
                .map(|gram| strsim::jaro_winkler(&title, gram))
                .fold(0.0, f64::max);
            (likeness, order)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut kept: Vec<usize> = scored
        .into_iter()
        .take(cap)
        .map(|(_, order)| order)
        .collect();
    kept.sort_unstable();
    kept.into_iter().map(|order| items[order].label()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn spans_cut_at_cue_words() {
        let spans = text_spans("Search for flights to Denver.");
        assert_eq!(spans[0], "Search for flights to Denver");
        assert!(spans.contains(&"flights to Denver".to_string()));
        assert!(spans.contains(&"Denver".to_string()));
    }

    #[test]
    fn spans_include_the_words_to_type() {
        let spans = text_spans("Type hello world");
        assert!(spans.contains(&"hello world".to_string()));
    }

    #[test]
    fn spans_prefer_quoted_text() {
        let spans = text_spans("Search for \u{201c}rust borrow checker\u{201d} please");
        assert_eq!(spans[1], "rust borrow checker");
    }

    #[test]
    fn spans_drop_the_leading_verb() {
        let spans = text_spans("Google best ramen in Oakland");
        assert!(spans.contains(&"best ramen in Oakland".to_string()));
    }

    #[test]
    fn spans_are_unique_and_capped() {
        let spans = text_spans("go to go to go to go to go to go to go to go to go to go to x");
        let unique: HashSet<String> = spans.iter().map(|s| s.to_lowercase()).collect();
        assert_eq!(unique.len(), spans.len());
        assert!(spans.len() <= MAX_SPANS);
    }

    #[test]
    fn cues_match_whole_words_only() {
        // "top" contains "to", "Safari" contains no cue.
        let spans = text_spans("open Safari on top");
        assert!(!spans.contains(&"p".to_string()));
        assert!(spans.contains(&"Safari on top".to_string()));
    }

    #[test]
    fn parses_numbers_in_digits_and_words() {
        assert_eq!(parse_number("Set the volume to 30%"), Some(30));
        assert_eq!(parse_number("volume fifty percent"), Some(50));
        assert_eq!(parse_number("volume to twenty-five"), Some(25));
        assert_eq!(parse_number("set it to one hundred"), Some(100));
        assert_eq!(parse_number("half volume"), Some(50));
        assert_eq!(parse_number("turn it all the way to max"), Some(100));
        assert_eq!(parse_number("volume seven"), Some(7));
        assert_eq!(parse_number("turn it up"), None);
    }

    #[test]
    fn shortlist_keeps_everything_under_the_cap() {
        let apps = shortlist_apps(
            &strings(&["Safari", "Slack", "Notes"]),
            &strings(&["Slack", "Terminal"]),
            "open slack",
            10,
        );
        assert_eq!(apps, strings(&["Slack", "Terminal", "Safari", "Notes"]));
    }

    fn menu_item(path: &[&str]) -> MenuItem {
        MenuItem {
            path: strings(path),
        }
    }

    #[test]
    fn menus_all_fit_under_the_cap() {
        let items = vec![
            menu_item(&["View", "Zoom In"]),
            menu_item(&["Mailbox", "Go To", "Inbox"]),
        ];
        assert_eq!(
            shortlist_menus(&items, "go to my inbox", 10),
            strings(&["View > Zoom In", "Mailbox > Go To > Inbox"])
        );
    }

    #[test]
    fn menus_over_the_cap_keep_the_likeliest_in_menu_order() {
        let mut items: Vec<MenuItem> = (0..300)
            .map(|i| menu_item(&["Format", &format!("Style {i}")]))
            .collect();
        items.insert(150, menu_item(&["View", "Show Downloads"]));
        items.push(menu_item(&["History", "Show All History"]));
        let kept = shortlist_menus(&items, "show my downloads", 20);
        assert_eq!(kept.len(), 20);
        assert!(kept.contains(&"View > Show Downloads".to_string()));
        let downloads = kept.iter().position(|l| l == "View > Show Downloads");
        let history = kept.iter().position(|l| l == "History > Show All History");
        if let (Some(downloads), Some(history)) = (downloads, history) {
            assert!(downloads < history, "menu order is kept");
        }
    }

    #[test]
    fn apps_named_aloud_are_matched_ignoring_spaces_and_case() {
        let apps = strings(&["TextEdit", "Safari", "System Settings", "Notes", "zoom.us"]);
        let said = |utterance: &str| named_app_command(utterance, &apps);
        let open = |app: &str| Some(("open_app", app.to_string()));
        assert_eq!(said("Open text edit."), open("TextEdit"));
        assert_eq!(said("please launch Safari"), open("Safari"));
        assert_eq!(
            said("Switch to the system settings app"),
            open("System Settings")
        );
        assert_eq!(said("quit notes"), Some(("quit_app", "Notes".to_string())));
        assert_eq!(
            said("Hide Safari for me"),
            Some(("hide_app", "Safari".to_string()))
        );
        assert_eq!(said("go ahead and open zoom.us"), open("zoom.us"));
        // Not an app, or more than an app: left to the model.
        assert_eq!(said("open trash"), None);
        assert_eq!(said("open the notes from yesterday"), None);
        assert_eq!(said("go to github.com"), None);
        assert_eq!(said("safari"), None);
        assert_eq!(said("open"), None);
        assert_eq!(said("Close this tab"), None);
    }

    fn action(id: &str, title: &str, what: &str, arg: ArgKind) -> Action {
        Action {
            id: id.into(),
            title: title.into(),
            what: what.into(),
            not_for: None,
            arg,
            runner: crate::voice_control::registry::Runner::TypeText,
        }
    }

    fn sample_actions() -> Vec<Action> {
        let mut actions = vec![
            action("mute", "Mute", "Mute the sound", ArgKind::None),
            action(
                "volume_up",
                "Volume up",
                "Turn the sound up or make it louder",
                ArgKind::None,
            ),
            action(
                "new_tab",
                "New tab",
                "Open a new browser tab",
                ArgKind::None,
            ),
            action(
                "lock_screen",
                "Lock screen",
                "Lock the screen",
                ArgKind::None,
            ),
            action(
                "menu_command",
                "Menu command",
                "Use one of the frontmost app's own menu commands",
                ArgKind::Menu,
            ),
        ];
        actions.extend((0..20).map(|i| {
            action(
                &format!("filler_{i}"),
                "Filler",
                "Something unrelated",
                ArgKind::None,
            )
        }));
        actions
    }

    #[test]
    fn action_shortlist_keeps_what_the_words_suggest() {
        let actions = sample_actions();
        let ids = |utterance: &str| -> Vec<String> {
            shortlist_actions(&actions, utterance, &[], 3)
                .iter()
                .map(|a| a.id.clone())
                .collect()
        };
        assert_eq!(ids("mute the sound")[0], "mute");
        assert!(ids("make it louder").contains(&"volume_up".to_string()));
        assert!(ids("open a new tab").contains(&"new_tab".to_string()));
        assert_eq!(ids("mute").len(), 3);
    }

    #[test]
    fn action_shortlist_keeps_everything_that_fits() {
        let actions = sample_actions();
        assert_eq!(
            shortlist_actions(&actions, "mute", &[], 100).len(),
            actions.len()
        );
    }

    #[test]
    fn menu_command_scores_by_the_menu_labels_on_offer() {
        let actions = sample_actions();
        let labels = vec!["View > Enlarge Text".to_string()];
        let rank = |labels: &[String]| {
            shortlist_actions(&actions, "enlarge the text", labels, 10)
                .iter()
                .position(|a| a.id == "menu_command")
                .unwrap_or(usize::MAX)
        };
        assert_eq!(rank(&labels), 0);
        assert!(rank(&[]) > 0);
    }

    #[test]
    fn shortlist_keeps_the_named_app_over_the_cap() {
        let installed: Vec<String> = (0..300)
            .map(|i| format!("Utility {i}"))
            .chain(["Visual Studio Code".to_string()])
            .collect();
        let apps = shortlist_apps(&installed, &[], "open visual studio code", 20);
        assert_eq!(apps.len(), 20);
        assert_eq!(apps[0], "Visual Studio Code");
    }
}
