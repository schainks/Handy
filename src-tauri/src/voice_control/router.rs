//! One Jev request per utterance, then a decision in code.
//!
//! The request asks up to five independent questions at once:
//!   is_command  noul    is this an instruction for the computer, or dictation?
//!   action      choice  which action, or none
//!   app         choice  which of the proposed apps is named, or none
//!   menu        choice  which of the frontmost app's menu commands, or none
//!   text        choice  which proposed span of the utterance is the argument
//! Code reads only the answers the chosen action needs.

use super::candidates::Proposal;
use super::context::DesktopContext;
use super::jev;
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
        is_command: response
            .answer("is_command")
            .and_then(|answer| answer.noul)
            .unwrap_or(0.0),
        action: picked("action"),
        action_confidence: response
            .answer("action")
            .and_then(|answer| answer.confidence)
            .unwrap_or(0.0),
        app: picked("app").filter(|app| proposal.apps.contains(app)),
        menu: picked("menu").filter(|label| proposal.menus.contains(label)),
        text,
        latency,
        model: response.model.clone(),
        input_tokens: response.usage.as_ref().map(|usage| usage.input_tokens),
    }
}

pub async fn route(
    client: &jev::Client,
    utterance: &str,
    ctx: &DesktopContext,
    actions: &[Action],
    proposal: &Proposal,
) -> Result<Route, String> {
    let (state, questions) = build_request(utterance, ctx, actions, proposal);
    let started = Instant::now();
    let response = client.ask(&state, &questions).await?;
    Ok(interpret(&response, proposal, started.elapsed()))
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
