use std::io::{BufRead, BufReader};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::Value;

use crate::auth;

#[derive(Debug, Clone)]
pub struct ChatClient {
    http: Client,
    model: String,
    instructions: String,
}

#[derive(Debug, Clone)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
struct ResponseRequest<'a> {
    model: &'a str,
    instructions: &'a str,
    store: bool,
    stream: bool,
    input: Vec<ResponseMessage<'a>>,
}

#[derive(Debug, Serialize)]
struct ResponseMessage<'a> {
    role: &'a str,
    content: Vec<ResponseContent<'a>>,
}

#[derive(Debug, Serialize)]
struct ResponseContent<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    text: &'a str,
}

enum ChatResponse {
    Complete(String),
    Unauthorized,
}

impl ChatClient {
    pub fn new(model: String, instructions: String) -> Result<Self> {
        let http = Client::builder()
            .user_agent("agent-may/0.1.0")
            .build()
            .context("failed to build the OpenAI client")?;

        Ok(Self {
            http,
            model,
            instructions,
        })
    }

    pub fn send(&self, turns: &[ChatTurn]) -> Result<String> {
        let request = ResponseRequest {
            model: &self.model,
            instructions: &self.instructions,
            store: false,
            stream: true,
            input: turns
                .iter()
                .map(|turn| ResponseMessage {
                    role: turn.role.as_str(),
                    content: vec![ResponseContent {
                        kind: content_type_for_role(&turn.role),
                        text: turn.content.as_str(),
                    }],
                })
                .collect(),
        };

        let mut session = auth::load_auth_session()?;
        match self.send_once(&request, &session)? {
            ChatResponse::Complete(text) => Ok(text),
            ChatResponse::Unauthorized => {
                auth::refresh_session_tokens(&mut session)?;
                match self.send_once(&request, &session)? {
                    ChatResponse::Complete(text) => Ok(text),
                    ChatResponse::Unauthorized => {
                        Err(anyhow!("chat request remained unauthorized after refresh"))
                    }
                }
            }
        }
    }

    fn send_once(&self, request: &ResponseRequest<'_>, session: &auth::AuthSession) -> Result<ChatResponse> {
        let access = auth::chatgpt_access(&session.auth)?;

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", access.access_token))
                .context("failed to construct the authorization header")?,
        );
        headers.insert(
            "chatgpt-account-id",
            HeaderValue::from_str(&access.account_id)
                .context("failed to construct the chatgpt-account-id header")?,
        );

        let response = self
            .http
            .post(auth::chatgpt_api_url("/codex/responses"))
            .headers(headers)
            .json(request)
            .send()
            .context("failed to send the chat request")?;

        if response.status() == StatusCode::UNAUTHORIZED {
            let body = response.text().unwrap_or_default();
            if !body.trim().is_empty() {
                eprintln!("refreshing ChatGPT auth after 401: {}", error_message(&body));
            }
            return Ok(ChatResponse::Unauthorized);
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .context("failed to read the chat error body")?;
            bail!("Codex backend returned {status}: {}", error_message(&body));
        }

        let text = extract_stream_text(response)?;
        Ok(ChatResponse::Complete(text))
    }
}

fn extract_stream_text(response: reqwest::blocking::Response) -> Result<String> {
    let reader = BufReader::new(response);
    let mut chunks = Vec::new();

    for line in reader.lines() {
        let line = line.context("failed while reading the SSE response")?;
        if line.is_empty() || !line.starts_with("data: ") {
            continue;
        }

        let payload = line.trim_start_matches("data: ").trim();
        if payload == "[DONE]" {
            break;
        }

        let event: Value =
            serde_json::from_str(payload).context("failed to parse an SSE event payload")?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    chunks.push(delta.to_string());
                }
            }
            Some("response.error") => {
                return Err(anyhow!("Codex backend reported an error: {}", payload));
            }
            _ => {}
        }
    }

    if chunks.is_empty() {
        return Err(anyhow!("chat response did not contain readable text"));
    }

    Ok(chunks.concat())
}

fn error_message(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "unknown error".to_string();
    }

    let Ok(json) = serde_json::from_str::<Value>(trimmed) else {
        return trimmed.to_string();
    };

    json.get("error")
        .and_then(Value::as_object)
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| json.get("detail").and_then(Value::as_str))
        .or_else(|| json.get("message").and_then(Value::as_str))
        .unwrap_or(trimmed)
        .to_string()
}

fn content_type_for_role(role: &str) -> &'static str {
    match role {
        "assistant" => "output_text",
        _ => "input_text",
    }
}
