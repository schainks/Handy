//! Minimal client for TypeSafe System One ("Jev").
//!
//! One `POST /v1/systemone` carries `{ state, model, questions }` and returns one
//! answer per question: a `choice` (the pick, its confidence and the full
//! probability distribution) or a `noul` (the probability of "yes"). Questions
//! in one request are answered independently, so the router asks everything it
//! might need in a single round trip and lets code ignore what it doesn't use.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

/// Pinned, because the `jev-latest` alias moves between releases.
pub const DEFAULT_MODEL: &str = "jev-1.13.0";
pub const DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

/// Which kind of System One server answers. Both speak the same wire format,
/// but a small local model needs a leaner request: a short state and no more
/// than a couple dozen options per question, where Jev takes hundreds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    #[default]
    Jev,
    Local,
}

impl Profile {
    /// `VOICE_COMMANDS_ENGINE` (`jev` or `local`) wins. Otherwise a model whose
    /// name isn't a Jev release ("typed-decisions", "clm-latest") is a local one.
    pub fn detect(model: &str) -> Self {
        match std::env::var("VOICE_COMMANDS_ENGINE")
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str()
        {
            "jev" => Self::Jev,
            "local" => Self::Local,
            _ => Self::for_model(model),
        }
    }

    pub fn for_model(model: &str) -> Self {
        if model.trim().to_lowercase().starts_with("jev") {
            Self::Jev
        } else {
            Self::Local
        }
    }
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    state: &'a Value,
    questions: &'a Value,
}

/// One answer. Which fields are present depends on the question type, so all
/// of them are optional and callers read the ones their question produces.
#[derive(Deserialize, Debug, Clone, Default)]
pub struct Answer {
    #[serde(default)]
    pub choice: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub noul: Option<f64>,
    /// A choice's probability for every option, when the server sends them.
    #[serde(default)]
    pub probabilities: HashMap<String, f64>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Response {
    pub answers: HashMap<String, Answer>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

impl Response {
    pub fn answer(&self, question: &str) -> Option<&Answer> {
        self.answers.get(question)
    }
}

pub fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))
}

pub struct Client {
    /// Shared across requests so a warm connection skips the TLS handshake.
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
    timeout: Duration,
    profile: Profile,
}

impl Client {
    pub fn new(
        http: reqwest::Client,
        endpoint: &str,
        api_key: &str,
        model: &str,
        timeout: Duration,
    ) -> Self {
        Self {
            http,
            endpoint: endpoint.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            timeout,
            profile: Profile::Jev,
        }
    }

    pub fn with_profile(mut self, profile: Profile) -> Self {
        self.profile = profile;
        self
    }

    pub fn profile(&self) -> Profile {
        self.profile
    }

    pub async fn ask(&self, state: &Value, questions: &Value) -> Result<Response, String> {
        let body = Request {
            model: &self.model,
            state,
            questions,
        };
        let response = self
            .http
            .post(&self.endpoint)
            .timeout(self.timeout)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    "Jev did not answer in time".to_string()
                } else {
                    format!("Jev request failed: {e}")
                }
            })?;

        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(format!("Jev returned {status}: {}", truncate(&detail, 300)));
        }

        response
            .json::<Response>()
            .await
            .map_err(|e| format!("Unreadable Jev response: {e}"))
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn local_models_are_recognized_by_name() {
        assert_eq!(Profile::for_model("jev-1.13.0"), Profile::Jev);
        assert_eq!(Profile::for_model("Jev-latest"), Profile::Jev);
        assert_eq!(Profile::for_model("typed-decisions"), Profile::Local);
        assert_eq!(Profile::for_model("clm-latest"), Profile::Local);
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Accept one HTTP request, hand back its raw text, reply with `body`.
    async fn serve_once(listener: TcpListener, status: &'static str, body: String) -> String {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw);
            if let Some(header_end) = text.find("\r\n\r\n") {
                let content_length = text[..header_end]
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if raw.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let reply = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(reply.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&raw).to_string()
    }

    #[tokio::test]
    async fn sends_the_wire_contract_and_parses_answers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let reply = json!({
            "answers": {
                "is_command": { "noul": 0.93 },
                "action": {
                    "choice": "open_app",
                    "confidence": 0.88,
                    "probabilities": { "open_app": 0.88, "none": 0.12 }
                }
            },
            "model": "jev-1.13.0",
            "usage": { "input_tokens": 812, "output_tokens": 40 }
        })
        .to_string();
        let server = tokio::spawn(serve_once(listener, "200 OK", reply));

        let client = Client::new(
            http_client().unwrap(),
            &endpoint,
            "test-key",
            DEFAULT_MODEL,
            Duration::from_secs(5),
        );
        let state = json!({ "utterance": "open safari" });
        let questions = json!({ "is_command": { "type": "noul", "instructions": "?" } });
        let response = client.ask(&state, &questions).await.unwrap();

        let request = server.await.unwrap();
        assert!(request.starts_with("POST /v1/systemone "));
        assert!(request
            .lines()
            .any(|line| line.eq_ignore_ascii_case("authorization: Bearer test-key")));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["model"], DEFAULT_MODEL);
        assert_eq!(body["state"], state);
        assert_eq!(body["questions"], questions);

        assert_eq!(response.answer("is_command").unwrap().noul, Some(0.93));
        let action = response.answer("action").unwrap();
        assert_eq!(action.choice.as_deref(), Some("open_app"));
        assert_eq!(action.confidence, Some(0.88));
        assert_eq!(response.usage.unwrap().input_tokens, 812);
    }

    #[tokio::test]
    async fn surfaces_http_errors_with_their_detail() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let server = tokio::spawn(serve_once(
            listener,
            "401 Unauthorized",
            r#"{"error":"invalid api key"}"#.to_string(),
        ));

        let client = Client::new(
            http_client().unwrap(),
            &endpoint,
            "bad-key",
            DEFAULT_MODEL,
            Duration::from_secs(5),
        );
        let error = client.ask(&json!({}), &json!({})).await.unwrap_err();
        server.await.unwrap();

        assert!(error.contains("401"), "{error}");
        assert!(error.contains("invalid api key"), "{error}");
    }

    #[test]
    fn truncates_long_error_bodies() {
        assert_eq!(truncate("  short  ", 10), "short");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }
}
