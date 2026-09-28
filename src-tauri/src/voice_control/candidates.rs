//! Code proposes, Jev chooses. Jev picks an action but doesn't write its
//! argument, so code offers the plausible arguments as options: app names from
//! the desktop, the frontmost app's menu commands, and spans cut from the
//! utterance at cue words ("search for", "type", "go to"...). Jev then picks
//! among them in the same request. Numbers are unambiguous enough to parse
//! outright.

use super::context::DesktopContext;
use super::menus::MenuItem;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;

/// One Choice over apps, well under Jev's 255-option limit.
pub const MAX_APP_CANDIDATES: usize = 200;
/// One Choice over menu commands. Most apps have fewer.
pub const MAX_MENU_CANDIDATES: usize = 200;
pub const MAX_SPANS: usize = 12;

#[derive(Debug, Clone, Default)]
pub struct Proposal {
    pub apps: Vec<String>,
    /// Labels of the frontmost app's menu commands ("View > Zoom In").
    pub menus: Vec<String>,
    pub spans: Vec<String>,
    pub number: Option<u32>,
}

pub fn propose(utterance: &str, ctx: &DesktopContext) -> Proposal {
    Proposal {
        apps: shortlist_apps(
            &ctx.installed_apps,
            &ctx.running_apps,
            utterance,
            MAX_APP_CANDIDATES,
        ),
        menus: shortlist_menus(&ctx.menu_items, utterance, MAX_MENU_CANDIDATES),
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
fn ngrams(utterance: &str) -> Vec<String> {
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
