//! Training rows for a local model, built by the app's own request code.
//!
//! A fine-tuned model must see in training exactly what Handy sends it: the
//! same shortlist of actions, the same option texts, the same state. So
//! labeled scenarios (an utterance, the desktop around it, and what it should
//! do) go through `build_local_request` and `build_local_target_request`, and
//! come out as `{state, questions, gold}` rows, the shape Laya's fine-tuning
//! reads.
//!
//! ```sh
//! HANDY_SCENARIOS=scenarios.jsonl HANDY_TRAINING_ROWS=rows.jsonl \
//!   cargo test --lib voice_control::training_data -- --ignored --nocapture
//! ```

use super::candidates::{self, LOCAL_MAX_ACTIONS, LOCAL_MAX_TARGETS};
use super::context::DesktopContext;
use super::elements;
use super::jev::Profile;
use super::menus::MenuItem;
use super::registry::{self, ArgKind};
use super::router;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Scenario {
    id: String,
    utterance: String,
    front: String,
    installed: Vec<String>,
    running: Vec<String>,
    #[serde(default)]
    menus: Vec<Vec<String>>,
    hammerspoon: bool,
    expect: Expect,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Expect {
    Dictation,
    Action {
        action: String,
        app: Option<String>,
        text: Option<String>,
        menu: Option<String>,
    },
    /// "Click …" on a screen of `(name, kind)` items. `target` indexes the
    /// item meant, or is absent when the screen has nothing by that name.
    Click {
        screen: Vec<(String, String)>,
        target: Option<usize>,
    },
}

fn one_hot(key: &str) -> Value {
    json!({ "label": key, "probabilities": { key: 1.0 } })
}

fn row(id: &str, kind: &str, state: &Value, questions: &Value, gold: Map<String, Value>) -> Value {
    json!({
        "id": format!("{id}:{kind}"),
        "workflow": "handy",
        "state": state.to_string(),
        "questions": questions.to_string(),
        "gold": Value::Object(gold).to_string(),
    })
}

#[derive(Default)]
struct Stats(BTreeMap<String, usize>);

impl Stats {
    fn add(&mut self, what: &str) {
        *self.0.entry(what.to_string()).or_default() += 1;
    }
}

fn rows_for(scenario: &Scenario, stats: &mut Stats) -> Vec<Value> {
    let ctx = DesktopContext {
        frontmost_app: Some(scenario.front.clone()),
        running_apps: scenario.running.clone(),
        installed_apps: scenario.installed.clone(),
        menu_items: scenario
            .menus
            .iter()
            .map(|path| MenuItem { path: path.clone() })
            .collect(),
        hammerspoon_cli: scenario
            .hammerspoon
            .then(|| PathBuf::from("/opt/homebrew/bin/hs")),
        ..Default::default()
    };
    let utterance = scenario.utterance.as_str();
    let actions = registry::builtin(&ctx);
    let proposal = candidates::propose(utterance, &ctx, Profile::Local);
    let shortlist =
        candidates::shortlist_actions(&actions, utterance, &proposal.menus, LOCAL_MAX_ACTIONS);
    let (state, questions) = router::build_local_request(utterance, &shortlist, &proposal);

    let mut gold = Map::new();
    let mut out = Vec::new();
    let (action, expected) = match &scenario.expect {
        Expect::Dictation => {
            gold.insert("action".into(), one_hot("none"));
            gold.insert("command".into(), one_hot("dictation"));
            stats.add("dictation");
            out.push(row(&scenario.id, "route", &state, &questions, gold));
            return out;
        }
        Expect::Action { action, .. } => (action.as_str(), &scenario.expect),
        Expect::Click { .. } => ("click_element", &scenario.expect),
    };

    // An action the shortlist missed can't be taught from this request.
    if !shortlist.iter().any(|candidate| candidate.id == action) {
        stats.add(&format!("skipped: {action} not in the shortlist"));
        return out;
    }
    gold.insert("action".into(), one_hot(action));
    gold.insert("command".into(), one_hot("command"));

    if let Expect::Action {
        app, text, menu, ..
    } = expected
    {
        let arg = actions
            .iter()
            .find(|candidate| candidate.id == action)
            .map(|candidate| candidate.arg);
        if arg == Some(ArgKind::App) {
            match app {
                Some(app) if proposal.apps.contains(app) => {
                    gold.insert("app".into(), one_hot(app));
                }
                _ => stats.add("skipped: app question, app not offered"),
            }
        }
        if arg == Some(ArgKind::Text) {
            let span = text.as_ref().and_then(|text| {
                proposal
                    .spans
                    .iter()
                    .position(|span| span.eq_ignore_ascii_case(text.trim()))
            });
            match span {
                Some(index) => {
                    gold.insert("text".into(), one_hot(&format!("t{}", index + 1)));
                }
                None => stats.add("skipped: text question, no span matches"),
            }
        }
        if arg == Some(ArgKind::Menu) {
            match menu {
                Some(label) if proposal.menus.contains(label) => {
                    gold.insert("menu".into(), one_hot(label));
                }
                _ => stats.add("skipped: menu question, label not offered"),
            }
        }
    }
    stats.add(&format!("action: {action}"));
    out.push(row(&scenario.id, "route", &state, &questions, gold));

    if let Expect::Click { screen, target } = expected {
        let named: Vec<(String, &str)> = screen
            .iter()
            .map(|(name, kind)| (name.clone(), kind.as_str()))
            .collect();
        let all_labels = elements::labels(&named);
        let names: Vec<String> = screen.iter().map(|(name, _)| name.clone()).collect();
        let offered = elements::shortlist(&names, utterance, LOCAL_MAX_TARGETS);
        let labels: Vec<String> = offered.iter().map(|&i| all_labels[i].clone()).collect();
        let key = match target {
            Some(index) if offered.contains(index) => all_labels[*index].clone(),
            Some(_) => {
                stats.add("skipped: click target not offered");
                return out;
            }
            None => "none".to_string(),
        };
        let (state, questions) = router::build_local_target_request(utterance, &labels);
        let mut gold = Map::new();
        gold.insert("target".into(), one_hot(&key));
        stats.add(if target.is_some() {
            "click target"
        } else {
            "click target: none"
        });
        out.push(row(&scenario.id, "target", &state, &questions, gold));
    }
    out
}

#[test]
#[ignore = "writes training rows; set HANDY_SCENARIOS and HANDY_TRAINING_ROWS"]
fn write_training_rows() {
    let input = std::env::var("HANDY_SCENARIOS").expect("HANDY_SCENARIOS is not set");
    let output = std::env::var("HANDY_TRAINING_ROWS").expect("HANDY_TRAINING_ROWS is not set");
    let text = std::fs::read_to_string(&input).expect("scenarios file");
    let mut stats = Stats::default();
    let mut lines = Vec::new();
    for (number, line) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let scenario: Scenario = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("scenario on line {}: {e}", number + 1));
        for row in rows_for(&scenario, &mut stats) {
            lines.push(row.to_string());
        }
    }
    std::fs::write(&output, lines.join("\n") + "\n").expect("write rows");
    println!("wrote {} rows to {output}", lines.len());
    for (what, count) in &stats.0 {
        println!("  {count:6}  {what}");
    }
}
