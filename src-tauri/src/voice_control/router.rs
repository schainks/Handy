//! One Jev request per utterance, then a decision in code.
//!
//! The request asks up to five independent questions at once:
//!   is_command  noul    is this an instruction for the computer, or dictation?
//!   action      choice  which action, or none
//!   app         choice  which of the proposed apps is named, or none
//!   menu        choice  which of the frontmost app's menu commands, or none
//!   text        choice  which proposed span of the utterance is the argument
//! Code reads only the answers the chosen action needs.
//!
//! A small local model gets a leaner request (`build_local_request`): the
//! utterance alone as the state, a shortlist of actions, and no `is_command`
//! question. Whether it was a command is read off the action question, as
//! the probability that the answer wasn't `none`.

use super::candidates::{self, Proposal, LOCAL_MAX_ACTIONS};
use super::context::DesktopContext;
use super::jev::{self, Profile};
use super::registry::{Action, ArgKind};
use serde_json::{json, Map, Value};
use std::time::{Duration, Instant};

const NONE: &str = "none";

/// Jev's reading of one utterance.
#[derive(Debug, Clone, Default)]
pub struct Route {
    /// P(the utterance is an instruction for the computer).
    pub is_command: f64,
    pub action: Option<String>,
    pub action_confidence: f64,
    pub app: Option<String>,
    /// Label of the frontmost app's menu command Jev picked.
    pub menu: Option<String>,
    /// How sure the model was of that menu command (its probability).
    pub menu_confidence: f64,
    pub text: Option<String>,
    pub latency: Duration,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum Decision {
    /// Paste the transcription as usual.
    Dictation,
    Run {
        action: Action,
        arg: Option<String>,
    },
    /// Clearly a command, but its argument wasn't heard (e.g. an app that
    /// isn't installed and no usable span).
    Unresolved {
        action: Action,
        reason: String,
    },
}

fn span_key(index: usize) -> String {
    format!("t{}", index + 1)
}

pub fn build_request(
    utterance: &str,
    ctx: &DesktopContext,
    actions: &[Action],
    proposal: &Proposal,
) -> (Value, Value) {
    let spans: Map<String, Value> = proposal
        .spans
        .iter()
        .enumerate()
        .map(|(i, span)| (span_key(i), json!(span)))
        .collect();

    let mut state = json!({
        "utterance": utterance,
        "frontmost_app": ctx.frontmost_app,
        "apps": proposal.apps,
        "text_spans": spans,
    });
    if !proposal.menus.is_empty() {
        state["menu_commands"] = json!(proposal.menus);
    }

    let mut action_criteria = Map::new();
    for action in actions {
        let criterion = match &action.not_for {
            Some(not_for) => json!({ "what": action.what, "not_for": not_for }),
            None => json!(action.what),
        };
        action_criteria.insert(action.id.clone(), criterion);
    }
    action_criteria.insert(
        NONE.into(),
        json!("None of the listed actions is what `utterance` asks for, or it is text to type"),
    );

    let mut questions = json!({
        "is_command": {
            "type": "noul",
            "instructions": "Is `utterance` the user telling the computer to do something right now, rather than dictating text to be typed into `frontmost_app`?",
            "criteria": {
                "true": "A spoken instruction to the computer, usually short and imperative: open or switch apps, move windows, change volume or media, control the browser, press a key, search the web, or type specific words",
                "false": "Text meant to be typed: a message, note, sentence, or answer, even if it contains words like open, close, search, or save"
            }
        },
        "action": {
            "type": "choice",
            "instructions": "Which action should the computer perform for `utterance`? Pick none if no listed action fits or if it is dictated text.",
            "criteria": action_criteria,
        },
    });

    if !proposal.apps.is_empty() {
        let mut app_criteria: Map<String, Value> = proposal
            .apps
            .iter()
            .map(|app| (app.clone(), Value::Null))
            .collect();
        app_criteria.insert(
            NONE.into(),
            json!("`utterance` does not name any application in `apps`"),
        );
        questions["app"] = json!({
            "type": "choice",
            "instructions": "If `utterance` names an application, which entry in `apps` is it? Speech recognition may have misspelled the name.",
            "criteria": app_criteria,
        });
    }

    if !proposal.menus.is_empty() {
        let mut menu_criteria: Map<String, Value> = proposal
            .menus
            .iter()
            .map(|label| (label.clone(), Value::Null))
            .collect();
        menu_criteria.insert(
            NONE.into(),
            json!("`utterance` asks for none of the entries in `menu_commands`"),
        );
        questions["menu"] = json!({
            "type": "choice",
            "instructions": "If `utterance` asks `frontmost_app` for something one of its menu commands does, which entry in `menu_commands` is it? Entries are menu paths, such as View > Show Sidebar.",
            "criteria": menu_criteria,
        });
    }

    if !proposal.spans.is_empty() {
        let mut span_criteria: Map<String, Value> = (0..proposal.spans.len())
            .map(|i| (span_key(i), Value::Null))
            .collect();
        span_criteria.insert(
            NONE.into(),
            json!("No entry in `text_spans` is exactly the needed text"),
        );
        questions["text"] = json!({
            "type": "choice",
            "instructions": "If the action needs text taken from `utterance` (a search query, a website, words to type, or the name of an app not in `apps`), which entry in `text_spans` is exactly that text, without the command words around it?",
            "criteria": span_criteria,
        });
    }

    (state, questions)
}

/// The lean request for a small local model. The state is the utterance
/// itself, since every extra line of context dilutes it, and every question
/// lists its options as text, since the model reads only that.
pub fn build_local_request(
    utterance: &str,
    actions: &[&Action],
    proposal: &Proposal,
) -> (Value, Value) {
    // The action question decides whether an app, menu command or text is
    // needed, so these only choose among the options; none isn't offered.
    let options = |labels: &[String]| -> Map<String, Value> {
        labels
            .iter()
            .map(|label| (label.clone(), json!(label)))
            .collect()
    };

    let mut action_criteria = Map::new();
    for action in actions {
        action_criteria.insert(action.id.clone(), json!(action.what));
    }
    action_criteria.insert(
        NONE.into(),
        json!("Not a computer command: text the user is dictating to be typed"),
    );
    let mut questions = json!({
        "action": {
            "type": "choice",
            "instructions": "What should the computer do for this utterance?",
            "criteria": action_criteria,
        },
        "command": {
            "type": "choice",
            "instructions": "Is this a command for the computer or text to type?",
            "criteria": {
                "command": "The user is telling the computer to do something now, such as open an app, click, scroll, change volume, press a key",
                "dictation": "The user is dictating text to be typed, such as a sentence, message or note",
            },
        },
    });

    if !proposal.apps.is_empty() {
        questions["app"] = json!({
            "type": "choice",
            "instructions": "Which application does the utterance name?",
            "criteria": options(&proposal.apps),
        });
    }
    if !proposal.menus.is_empty() {
        questions["menu"] = json!({
            "type": "choice",
            "instructions": "Which menu command does the utterance ask for? Entries are menu paths.",
            "criteria": options(&proposal.menus),
        });
    }
    if !proposal.spans.is_empty() {
        let mut criteria = Map::new();
        for (i, span) in proposal.spans.iter().enumerate() {
            criteria.insert(span_key(i), json!(span));
        }
        questions["text"] = json!({
            "type": "choice",
            "instructions": "Which part of the utterance is the text to search for, open or type?",
            "criteria": criteria,
        });
    }
    (json!(utterance), questions)
}

/// A local model's own signals are weak alone, but the probability that the
/// action wasn't `none`, times the probability that it's a command rather than
/// dictation, less a penalty per word (commands are short, dictation isn't),
/// separates them well. The offset puts the operating point at the default
/// 0.7 threshold, so that setting keeps its meaning. Fit on a held-out set of
/// 158 commands and 60 tricky dictations.
const LOCAL_GATE_WORDS: f64 = 40.0;
const LOCAL_GATE_OFFSET: f64 = 0.48;

fn local_gate(response: &jev::Response, utterance: &str) -> f64 {
    let action = command_probability(response);
    let command = response
        .answer("command")
        .and_then(|answer| answer.probabilities.get("command").copied())
        .unwrap_or(1.0);
    let words = utterance.split_whitespace().count() as f64;
    (action * command - words / LOCAL_GATE_WORDS + LOCAL_GATE_OFFSET).clamp(0.0, 1.0)
}

/// "Click this", "click here", "press that one": the pointer click, which is a
/// closed set of phrases, so a small local model isn't asked.
fn is_pointer_click(utterance: &str) -> bool {
    let lower = utterance.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    matches!(
        words.as_slice(),
        ["click" | "tap" | "press", "this" | "here" | "that" | "it"]
            | ["click" | "tap" | "press", "this" | "that", "one"]
    )
}

/// How sure a local model must be of a menu command before it is pressed.
const LOCAL_MIN_MENU_CONFIDENCE: f64 = 0.75;

/// Sentences that are plainly said to a person, not to the computer: "Let's run
/// this whole test", "Can we do left justified text?", "I think we should...".
/// The trained model is overconfident on these, so a few openings that
/// commands don't have decide it outright. "I want you to open Safari" is a
/// command and stays one.
fn looks_like_dictation(utterance: &str) -> bool {
    let lower = utterance.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|word| !word.is_empty())
        .collect();
    if words.len() < 4 {
        return false;
    }
    const ASKING_YOU_TO: &[&[&str]] = &[
        &["i", "want", "you", "to"],
        &["i", "need", "you", "to"],
        &["i'd", "like", "you", "to"],
        &["i", "would", "like", "you", "to"],
    ];
    if ASKING_YOU_TO
        .iter()
        .any(|opening| words.starts_with(opening))
    {
        return false;
    }
    const OPENINGS: &[&[&str]] = &[
        &["can", "we"],
        &["could", "we"],
        &["should", "we"],
        &["shall", "we"],
        &["do", "we"],
        &["are", "we"],
        &["is", "it"],
        &["is", "there"],
        &["do", "you"],
        &["did", "you"],
        &["are", "you"],
        &["have", "you"],
        &["this", "is"],
        &["that", "is"],
    ];
    const FIRST_WORDS: &[&str] = &[
        "let's", "lets", "why", "how", "who", "whose", "when", "where", "which", "what", "we",
        "we're", "i", "i'm", "i've", "i'll", "it's", "that's", "there's", "here's", "they", "he",
        "she", "my", "our",
    ];
    OPENINGS.iter().any(|opening| words.starts_with(opening)) || FIRST_WORDS.contains(&words[0])
}

/// A small model asked "which app?" always names one, even when the utterance
/// named none ("turn off Wi Fi" became "quit MacWhisper"). An action that acts
/// on an app only runs if that app was actually spoken; otherwise it has no
/// app and is reported as not understood, rather than guessed at.
fn require_spoken_app(route: &mut Route, utterance: &str, actions: &[Action]) {
    let takes_app = route
        .action
        .as_ref()
        .and_then(|id| actions.iter().find(|action| &action.id == id))
        .is_some_and(|action| action.arg == ArgKind::App);
    if !takes_app {
        return;
    }
    if !route
        .app
        .as_deref()
        .is_some_and(|app| candidates::app_was_spoken(app, utterance))
    {
        route.app = None;
        route.text = None;
    }
}

/// P(command): a server that asks `is_command` answers it directly. A local
/// one is read off the action question, as the probability that the answer
/// wasn't `none`.
fn command_probability(response: &jev::Response) -> f64 {
    if let Some(noul) = response.answer("is_command").and_then(|a| a.noul) {
        return noul;
    }
    let Some(action) = response.answer("action") else {
        return 0.0;
    };
    match action.probabilities.get(NONE) {
        Some(none) => (1.0 - none).clamp(0.0, 1.0),
        None => f64::from(action.choice.as_deref().is_some_and(|c| c != NONE)),
    }
}

pub fn interpret(response: &jev::Response, proposal: &Proposal, latency: Duration) -> Route {
    let picked = |question: &str| {
        response
            .answer(question)
            .and_then(|answer| answer.choice.clone())
            .filter(|choice| choice != NONE)
    };

    let text = picked("text").and_then(|key| {
        let index = key
            .strip_prefix('t')?
            .parse::<usize>()
            .ok()?
            .checked_sub(1)?;
        proposal.spans.get(index).cloned()
    });

    Route {
        is_command: command_probability(response),
        action: picked("action"),
        action_confidence: response
            .answer("action")
            .and_then(|answer| answer.confidence)
            .unwrap_or(0.0),
        app: picked("app").filter(|app| proposal.apps.contains(app)),
        menu: picked("menu").filter(|label| proposal.menus.contains(label)),
        menu_confidence: response
            .answer("menu")
            .map(|answer| {
                answer
                    .choice
                    .as_ref()
                    .and_then(|label| answer.probabilities.get(label).copied())
                    .or(answer.confidence)
                    .unwrap_or(0.0)
            })
            .unwrap_or(0.0),
        text,
        latency,
        model: response.model.clone(),
        input_tokens: response.usage.as_ref().map(|usage| usage.input_tokens),
    }
}

/// The second request for "click …", made only then: which of the items on
/// screen the utterance means.
pub fn build_target_request(
    utterance: &str,
    ctx: &DesktopContext,
    labels: &[String],
) -> (Value, Value) {
    let state = json!({
        "utterance": utterance,
        "frontmost_app": ctx.frontmost_app,
        "on_screen": labels,
    });
    let mut criteria: Map<String, Value> = labels
        .iter()
        .map(|label| (label.clone(), Value::Null))
        .collect();
    criteria.insert(
        NONE.into(),
        json!("`utterance` names none of the items in `on_screen`"),
    );
    let questions = json!({
        "target": {
            "type": "choice",
            "instructions": "Which item in `on_screen` does `utterance` ask to click, press, open or select? Each entry is an item's text and its kind, such as Octopus (link) or Search Wikipedia (field). Items that share a name are numbered after the kind, in screen order.",
            "criteria": criteria,
        }
    });
    (state, questions)
}

/// The lean form of the click request for a small local model.
pub fn build_local_target_request(utterance: &str, labels: &[String]) -> (Value, Value) {
    let mut criteria: Map<String, Value> = labels
        .iter()
        .map(|label| (label.clone(), json!(label)))
        .collect();
    criteria.insert(
        NONE.into(),
        json!("Nothing in this list is what the utterance asks to click"),
    );
    let questions = json!({
        "target": {
            "type": "choice",
            "instructions": "Which item on the screen does the utterance ask to click, press, open or select? Each item is its text and its kind.",
            "criteria": criteria,
        }
    });
    (json!(utterance), questions)
}

/// The label picked and how sure the model is, if it picked one of `labels`.
/// Jev reports its own confidence. A local model is as sure as the
/// probability it gave the pick.
pub fn interpret_target(
    response: &jev::Response,
    labels: &[String],
    profile: Profile,
) -> Option<(String, f64)> {
    let answer = response.answer("target")?;
    let label = answer
        .choice
        .clone()
        .filter(|label| label != NONE && labels.contains(label))?;
    let confidence = match profile {
        Profile::Jev => answer.confidence,
        Profile::Local => answer.probabilities.get(&label).copied(),
    };
    Some((label, confidence.unwrap_or(0.0)))
}

pub async fn pick_target(
    client: &jev::Client,
    utterance: &str,
    ctx: &DesktopContext,
    labels: &[String],
) -> Result<Option<(String, f64)>, String> {
    let (state, questions) = match client.profile() {
        Profile::Jev => build_target_request(utterance, ctx, labels),
        Profile::Local => build_local_target_request(utterance, labels),
    };
    let response = client.ask(&state, &questions).await?;
    Ok(interpret_target(&response, labels, client.profile()))
}

pub async fn route(
    client: &jev::Client,
    utterance: &str,
    ctx: &DesktopContext,
    actions: &[Action],
    proposal: &Proposal,
) -> Result<Route, String> {
    if client.profile() == Profile::Local {
        if is_pointer_click(utterance) {
            return Ok(Route {
                is_command: 1.0,
                action: Some("click_pointer".to_string()),
                action_confidence: 1.0,
                ..Default::default()
            });
        }
        let apps: Vec<String> = ctx
            .running_apps
            .iter()
            .chain(&ctx.installed_apps)
            .cloned()
            .collect();
        if let Some((action, app)) = candidates::named_app_command(utterance, &apps) {
            return Ok(Route {
                is_command: 1.0,
                action: Some(action.to_string()),
                action_confidence: 1.0,
                app: Some(app),
                ..Default::default()
            });
        }
    }
    let (state, questions) = match client.profile() {
        Profile::Jev => build_request(utterance, ctx, actions, proposal),
        Profile::Local => {
            let shortlist = candidates::shortlist_actions(
                actions,
                utterance,
                &proposal.menus,
                LOCAL_MAX_ACTIONS,
            );
            build_local_request(utterance, &shortlist, proposal)
        }
    };
    let started = Instant::now();
    let response = client.ask(&state, &questions).await?;
    let mut route = interpret(&response, proposal, started.elapsed());
    if client.profile() == Profile::Local {
        route.is_command = local_gate(&response, utterance);
        if looks_like_dictation(utterance) {
            route.is_command = 0.0;
        }
        require_spoken_app(&mut route, utterance, actions);
        // The menu question has no none option, so it always picks something:
        // "open text" became Sort By > None. A pick it isn't sure of is dropped.
        if route.menu_confidence < LOCAL_MIN_MENU_CONFIDENCE {
            route.menu = None;
        }
    }
    Ok(route)
}

/// Run the chosen action whenever Jev is at least `threshold` sure the
/// utterance was a command; everything else is dictation.
pub fn decide(route: &Route, actions: &[Action], proposal: &Proposal, threshold: f64) -> Decision {
    if route.is_command < threshold {
        return Decision::Dictation;
    }
    let Some(action) = route
        .action
        .as_ref()
        .and_then(|id| actions.iter().find(|action| &action.id == id))
    else {
        return Decision::Dictation;
    };

    let (arg, missing) = match action.arg {
        ArgKind::None => (None, ""),
        // An app Jev couldn't match (not installed, or unusually named) still
        // gets a try under the name that was said.
        ArgKind::App => (
            route.app.clone().or_else(|| route.text.clone()),
            "no app name was heard",
        ),
        ArgKind::Text => (route.text.clone(), "the text to use was not clear"),
        ArgKind::Number => (
            proposal.number.map(|n| n.to_string()),
            "no number was heard",
        ),
        ArgKind::Menu => (route.menu.clone(), "no matching menu command was found"),
    };

    if action.arg != ArgKind::None && arg.is_none() {
        return Decision::Unresolved {
            action: action.clone(),
            reason: missing.to_string(),
        };
    }
    Decision::Run {
        action: action.clone(),
        arg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_control::registry::builtin;
    use std::collections::HashMap;

    fn ctx() -> DesktopContext {
        DesktopContext {
            frontmost_app: Some("Notes".into()),
            installed_apps: vec!["Safari".into(), "Slack".into()],
            ..Default::default()
        }
    }

    fn proposal() -> Proposal {
        Proposal {
            apps: vec!["Safari".into(), "Slack".into()],
            menus: vec!["View > Zoom In".into(), "View > Show Downloads".into()],
            spans: vec!["search for cats".into(), "cats".into()],
            number: Some(30),
        }
    }

    /// Safari in front, with two menu commands read.
    fn safari() -> DesktopContext {
        DesktopContext {
            frontmost_app: Some("Safari".into()),
            frontmost_pid: Some(42),
            menu_items: ["Zoom In", "Show Downloads"]
                .iter()
                .map(|title| crate::voice_control::menus::MenuItem {
                    path: vec!["View".into(), title.to_string()],
                })
                .collect(),
            ..ctx()
        }
    }

    fn response(answers: &[(&str, jev::Answer)]) -> jev::Response {
        jev::Response {
            answers: answers
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect::<HashMap<_, _>>(),
            model: Some("jev-1.13.0".into()),
            usage: None,
        }
    }

    fn choice(pick: &str) -> jev::Answer {
        jev::Answer {
            choice: Some(pick.into()),
            confidence: Some(0.9),
            ..Default::default()
        }
    }

    fn noul(p: f64) -> jev::Answer {
        jev::Answer {
            noul: Some(p),
            ..Default::default()
        }
    }

    /// A choice answer with its probabilities, as a local model sends them.
    fn distribution(pick: &str, probabilities: &[(&str, f64)]) -> jev::Answer {
        jev::Answer {
            choice: Some(pick.into()),
            probabilities: probabilities
                .iter()
                .map(|(k, p)| (k.to_string(), *p))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn local_request_is_lean_and_lists_its_options_as_text() {
        let actions = builtin(&safari());
        let shortlist =
            candidates::shortlist_actions(&actions, "show my downloads", &proposal().menus, 14);
        let (state, questions) = build_local_request("show my downloads", &shortlist, &proposal());

        assert_eq!(state, json!("show my downloads"));
        assert!(questions.get("is_command").is_none());
        assert!(questions.get("command").is_some());
        let options = questions["action"]["criteria"].as_object().unwrap();
        assert_eq!(options.len(), 14 + 1, "the shortlist plus none");
        assert!(options.contains_key("menu_command"));
        assert!(options["none"].as_str().unwrap().contains("dictating"));
        // Every option carries its own text, since the model reads only that.
        let apps = questions["app"]["criteria"].as_object().unwrap();
        assert_eq!(apps["Safari"], json!("Safari"));
        let spans = questions["text"]["criteria"].as_object().unwrap();
        assert_eq!(spans["t2"], json!("cats"));
    }

    #[test]
    fn local_gate_needs_a_command_that_is_short() {
        let answers = |none: f64, command: f64| {
            response(&[
                (
                    "action",
                    distribution("mute", &[("mute", 1.0 - none), ("none", none)]),
                ),
                (
                    "command",
                    distribution(
                        "command",
                        &[("command", command), ("dictation", 1.0 - command)],
                    ),
                ),
            ])
        };
        // A moderately sure model is settled by length.
        let unsure = answers(0.3, 0.6);
        assert!(local_gate(&unsure, "mute the sound") > 0.7);
        assert!(
            local_gate(
                &unsure,
                "the volume of sales went up last month and we should mute the noise"
            ) < 0.7
        );
        assert!(local_gate(&answers(0.02, 0.9), "mute the sound") > 0.7);
        // A model that leans toward none or dictation is not sure enough.
        assert!(local_gate(&answers(0.8, 0.9), "mute the sound") < 0.7);
        assert!(local_gate(&answers(0.02, 0.2), "mute the sound") < 0.7);
        assert!((0.0..=1.0).contains(&local_gate(&answers(0.0, 1.0), "mute")));
    }

    #[test]
    fn a_local_target_is_as_sure_as_its_probability() {
        let labels = vec!["Octopus (link)".to_string(), "Squid (link)".to_string()];
        let answer = jev::Answer {
            confidence: Some(0.95),
            ..distribution("Octopus (link)", &[("Octopus (link)", 0.4), ("none", 0.6)])
        };
        let response = response(&[("target", answer)]);
        assert_eq!(
            interpret_target(&response, &labels, Profile::Local),
            Some(("Octopus (link)".to_string(), 0.4))
        );
        assert_eq!(
            interpret_target(&response, &labels, Profile::Jev),
            Some(("Octopus (link)".to_string(), 0.95))
        );
    }

    #[test]
    fn sentences_said_to_a_person_are_dictation() {
        for yes in [
            "Let's run this whole test.",
            "Can we do left justified text?",
            "Why is the justification on the right?",
            "I think we should open the discussion first.",
            "We should close the deal before Friday.",
            "It's a beautiful day to go outside.",
            "This is a demo of using Handy offline.",
        ] {
            assert!(looks_like_dictation(yes), "{yes}");
        }
        for no in [
            "Open Safari.",
            "Can you open Safari?",
            "I want you to lock the screen.",
            "Turn the Wi Fi off.",
            "Move this window to the left half.",
            "A bit louder.",
            "Quit text edit.",
            "Hey, mute it please.",
            "Show my downloads.",
        ] {
            assert!(!looks_like_dictation(no), "{no}");
        }
    }

    #[test]
    fn an_app_nobody_mentioned_is_dropped() {
        let actions = builtin(&ctx());
        let route = |app: &str| Route {
            action: Some("quit_app".into()),
            app: Some(app.into()),
            text: Some("Wi Fi off".into()),
            ..Default::default()
        };
        let mut guessed = route("MacWhisper");
        require_spoken_app(&mut guessed, "Turn the Wi Fi off.", &actions);
        assert_eq!((guessed.app.clone(), guessed.text.clone()), (None, None));
        // With no app there is nothing to run: the decision is "not understood".
        let proposal = proposal();
        let decision = decide(&guessed, &actions, &proposal, 0.0);
        assert!(matches!(decision, Decision::Unresolved { .. }));

        let mut said = route("Safari");
        require_spoken_app(&mut said, "quit Safari please", &actions);
        assert_eq!(said.app.as_deref(), Some("Safari"));
        // Actions that take no app are left alone.
        let mut mute = Route {
            action: Some("mute".into()),
            app: Some("Notes".into()),
            ..Default::default()
        };
        require_spoken_app(&mut mute, "mute", &actions);
        assert_eq!(mute.app.as_deref(), Some("Notes"));
    }

    #[test]
    fn pointer_clicks_are_a_closed_set_of_phrases() {
        for yes in [
            "Click this",
            "click here.",
            "Press that one",
            "tap it",
            "Click that!",
        ] {
            assert!(is_pointer_click(yes), "{yes}");
        }
        for no in [
            "click cephalopods",
            "click here to unsubscribe",
            "click",
            "this is a test",
        ] {
            assert!(!is_pointer_click(no), "{no}");
        }
    }

    #[test]
    fn local_argument_questions_offer_no_none() {
        let actions = builtin(&ctx());
        let shortlist = candidates::shortlist_actions(&actions, "open safari", &[], 14);
        let (_, questions) = build_local_request("open safari", &shortlist, &proposal());
        for question in ["app", "menu", "text"] {
            let options = questions[question]["criteria"].as_object().unwrap();
            assert!(!options.contains_key("none"), "{question}");
        }
        assert!(questions["action"]["criteria"]
            .as_object()
            .unwrap()
            .contains_key("none"));
    }

    #[test]
    fn local_target_request_states_only_the_utterance() {
        let labels = vec!["Octopus (link)".to_string()];
        let (state, questions) = build_local_target_request("click octopus", &labels);
        assert_eq!(state, json!("click octopus"));
        let options = questions["target"]["criteria"].as_object().unwrap();
        assert_eq!(options["Octopus (link)"], json!("Octopus (link)"));
        assert!(options.contains_key("none"));
    }

    fn route_for(answers: &[(&str, jev::Answer)]) -> Route {
        interpret(&response(answers), &proposal(), Duration::ZERO)
    }

    #[test]
    fn request_carries_state_and_all_four_questions() {
        let actions = builtin(&ctx());
        let (state, questions) = build_request("search for cats", &ctx(), &actions, &proposal());

        assert_eq!(state["utterance"], "search for cats");
        assert_eq!(state["frontmost_app"], "Notes");
        assert_eq!(state["apps"], json!(["Safari", "Slack"]));
        assert_eq!(state["text_spans"]["t2"], "cats");

        assert_eq!(questions["is_command"]["type"], "noul");
        let action_criteria = questions["action"]["criteria"].as_object().unwrap();
        assert_eq!(action_criteria.len(), actions.len() + 1);
        assert!(action_criteria.contains_key(NONE));
        assert!(action_criteria["open_app"]["not_for"].is_string());
        assert!(action_criteria["mute"].is_string());
        assert_eq!(
            questions["app"]["criteria"].as_object().unwrap().len(),
            3,
            "two apps plus none"
        );
        assert!(questions["app"]["criteria"]["Safari"].is_null());
        assert!(questions["text"]["criteria"]["t1"].is_null());
    }

    #[test]
    fn request_skips_questions_without_candidates() {
        let empty = Proposal::default();
        let (state, questions) = build_request("mute", &ctx(), &builtin(&ctx()), &empty);
        assert!(questions.get("app").is_none());
        assert!(questions.get("menu").is_none());
        assert!(questions.get("text").is_none());
        assert!(state.get("menu_commands").is_none());
    }

    #[test]
    fn request_offers_the_frontmost_apps_menu_commands() {
        let actions = builtin(&safari());
        let (state, questions) = build_request("show downloads", &safari(), &actions, &proposal());
        assert_eq!(
            state["menu_commands"],
            json!(["View > Zoom In", "View > Show Downloads"])
        );
        let menu_criteria = questions["menu"]["criteria"].as_object().unwrap();
        assert_eq!(menu_criteria.len(), 3, "two menu commands plus none");
        assert!(menu_criteria["View > Show Downloads"].is_null());
        assert!(questions["action"]["criteria"]["menu_command"]["not_for"].is_string());
    }

    #[test]
    fn target_request_offers_the_items_on_screen() {
        let labels = vec![
            "Octopus (link)".to_string(),
            "Search Wikipedia (field)".to_string(),
        ];
        let (state, questions) = build_target_request("click octopus", &safari(), &labels);
        assert_eq!(state["on_screen"], json!(labels));
        assert_eq!(state["frontmost_app"], "Safari");
        let criteria = questions["target"]["criteria"].as_object().unwrap();
        assert_eq!(criteria.len(), 3, "two items plus none");
        assert!(criteria["Octopus (link)"].is_null());

        let picked = |pick: &str| {
            interpret_target(
                &response(&[("target", choice(pick))]),
                &labels,
                Profile::Jev,
            )
        };
        assert_eq!(
            picked("Octopus (link)"),
            Some(("Octopus (link)".to_string(), 0.9))
        );
        assert_eq!(picked(NONE), None);
        assert_eq!(
            picked("Squid (link)"),
            None,
            "only listed items are clicked"
        );
    }

    #[test]
    fn runs_the_menu_command_jev_picked() {
        let actions = builtin(&safari());
        let answers = [
            ("is_command", noul(0.9)),
            ("action", choice("menu_command")),
            ("menu", choice("View > Show Downloads")),
        ];
        match decide(&route_for(&answers), &actions, &proposal(), 0.7) {
            Decision::Run { action, arg } => {
                assert_eq!(action.id, "menu_command");
                assert_eq!(arg.as_deref(), Some("View > Show Downloads"));
            }
            other => panic!("expected a run, got {other:?}"),
        }

        let unknown = route_for(&[("menu", choice("File > Not Read"))]);
        assert_eq!(
            unknown.menu, None,
            "only proposed menu commands are accepted"
        );

        let no_match = [
            ("is_command", noul(0.9)),
            ("action", choice("menu_command")),
            ("menu", choice(NONE)),
        ];
        assert!(matches!(
            decide(&route_for(&no_match), &actions, &proposal(), 0.7),
            Decision::Unresolved { .. }
        ));
    }

    #[test]
    fn interprets_answers_and_maps_span_keys_back_to_text() {
        let route = route_for(&[
            ("is_command", noul(0.97)),
            ("action", choice("web_search")),
            ("app", choice(NONE)),
            ("text", choice("t2")),
        ]);
        assert_eq!(route.is_command, 0.97);
        assert_eq!(route.action.as_deref(), Some("web_search"));
        assert_eq!(route.app, None);
        assert_eq!(route.text.as_deref(), Some("cats"));

        let out_of_range = route_for(&[("text", choice("t9")), ("app", choice("Chrome"))]);
        assert_eq!(out_of_range.text, None);
        assert_eq!(out_of_range.app, None, "only proposed apps are accepted");
        assert_eq!(
            out_of_range.is_command, 0.0,
            "missing answers are not commands"
        );
    }

    #[test]
    fn low_command_probability_is_dictation() {
        let actions = builtin(&ctx());
        let route = route_for(&[("is_command", noul(0.4)), ("action", choice("mute"))]);
        assert!(matches!(
            decide(&route, &actions, &proposal(), 0.7),
            Decision::Dictation
        ));
    }

    #[test]
    fn a_command_with_no_matching_action_is_dictation() {
        let actions = builtin(&ctx());
        for pick in [NONE, "not_an_action"] {
            let route = route_for(&[("is_command", noul(0.9)), ("action", choice(pick))]);
            assert!(matches!(
                decide(&route, &actions, &proposal(), 0.7),
                Decision::Dictation
            ));
        }
    }

    #[test]
    fn runs_with_the_argument_the_action_needs() {
        let actions = builtin(&ctx());
        let run = |answers: &[(&str, jev::Answer)]| match decide(
            &route_for(answers),
            &actions,
            &proposal(),
            0.7,
        ) {
            Decision::Run { action, arg } => (action.id, arg),
            other => panic!("expected a run, got {other:?}"),
        };

        assert_eq!(
            run(&[("is_command", noul(0.9)), ("action", choice("mute"))]),
            ("mute".into(), None)
        );
        assert_eq!(
            run(&[
                ("is_command", noul(0.9)),
                ("action", choice("open_app")),
                ("app", choice("Slack")),
            ]),
            ("open_app".into(), Some("Slack".into()))
        );
        assert_eq!(
            run(&[
                ("is_command", noul(0.9)),
                ("action", choice("open_app")),
                ("app", choice(NONE)),
                ("text", choice("t2")),
            ]),
            ("open_app".into(), Some("cats".into())),
            "an unmatched app falls back to the spoken name"
        );
        assert_eq!(
            run(&[
                ("is_command", noul(0.9)),
                ("action", choice("web_search")),
                ("text", choice("t2")),
            ]),
            ("web_search".into(), Some("cats".into()))
        );
        assert_eq!(
            run(&[("is_command", noul(0.9)), ("action", choice("set_volume"))]),
            ("set_volume".into(), Some("30".into()))
        );
    }

    #[test]
    fn a_command_missing_its_argument_is_unresolved() {
        let actions = builtin(&ctx());
        let route = route_for(&[
            ("is_command", noul(0.9)),
            ("action", choice("web_search")),
            ("text", choice(NONE)),
        ]);
        match decide(&route, &actions, &proposal(), 0.7) {
            Decision::Unresolved { action, reason } => {
                assert_eq!(action.id, "web_search");
                assert!(reason.contains("text"));
            }
            other => panic!("expected unresolved, got {other:?}"),
        }
    }
}
