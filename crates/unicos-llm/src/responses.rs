use crate::LlmProvider;
use async_trait::async_trait;
use reqwest::header::AUTHORIZATION;
use serde::Deserialize;
use serde_json::Value;
use std::env;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use unicos_common::{ChatMessage, ChatRole, Perception, Sender, UnicError};

#[derive(Clone, Debug)]
pub struct ResponsesProvider {
    pub base_url: String,
    pub api_key_env: String,
    pub api_key: Option<String>,
    pub model: String,
    pub timeout_ms: u64,
    pub max_retries: usize,
}

impl ResponsesProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key_env: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key_env: api_key_env.into(),
            api_key,
            model: model.into(),
            timeout_ms: 60_000,
            max_retries: 2,
        }
    }

    fn api_key(&self) -> Result<String, UnicError> {
        if let Ok(key) = env::var(&self.api_key_env) {
            if !key.trim().is_empty() {
                return Ok(key);
            }
        }
        if let Some(key) = self.api_key.as_deref() {
            if !key.trim().is_empty() {
                return Ok(key.to_string());
            }
        }
        Err(UnicError::Llm(format!(
            "missing api key: set env var {} or configure llm.responses.api_key",
            self.api_key_env
        )))
    }

    fn prompt_to_input(prompt: Vec<ChatMessage>) -> Value {
        // Keep it simple and provider-agnostic: concatenate into a single user input.
        // This matches OpenAI-compatible /responses servers that accept string input.
        let mut out = String::new();
        for msg in prompt {
            let role = match msg.role {
                ChatRole::System => "system",
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
                ChatRole::Tool => "tool",
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(role);
            out.push_str(": ");
            out.push_str(msg.content.trim());
        }
        Value::String(out)
    }

    fn perception_to_gate_input(perception: &Perception) -> Value {
        let mut system = String::new();
        system.push_str("You are a decision gate for an autonomous agent.\n");
        system.push_str("Decide whether the agent should respond to the latest user message.\n");
        system.push_str("Output exactly YES or NO (no extra words).\n");
        system.push_str("Do NOT output the agent's actual reply.\n");

        let mut user = String::new();
        if !perception.soul.trim().is_empty() {
            user.push_str("AGENT_SOUL:\n");
            user.push_str(perception.soul.trim());
            user.push_str("\n\n");
        }
        user.push_str("RECENT_MESSAGES:\n");
        for m in &perception.recent {
            let role = match m.sender {
                Sender::UserSudo => "user",
                Sender::Agent(_) => "assistant",
                Sender::System => "system",
            };
            user.push_str(role);
            user.push_str(": ");
            user.push_str(m.payload.text.trim());
            user.push('\n');
        }
        user.push_str("\nQuestion: Should the agent respond now? Answer YES or NO.\n");

        serde_json::json!([
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ])
    }

    fn extract_output_text(response_json: &Value) -> Option<String> {
        if let Some(output_text) = response_json.get("output_text").and_then(Value::as_str) {
            return Some(output_text.to_string());
        }

        let output_items = response_json.get("output")?.as_array()?;
        let mut out = String::new();

        for item in output_items {
            let contents = match item.get("content").and_then(Value::as_array) {
                Some(contents) => contents,
                None => continue,
            };

            for part in contents {
                let ty = part.get("type").and_then(Value::as_str);
                if !matches!(ty, Some("output_text" | "text")) {
                    continue;
                }
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
        }

        if out.is_empty() { None } else { Some(out) }
    }

    async fn responses_create(&self, input: Value) -> Result<String, UnicError> {
        let api_key = self.api_key()?;
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));

        let body = serde_json::json!({
            "model": self.model,
            "input": input,
        });

        let client = reqwest::Client::new();
        let mut last_err: Option<UnicError> = None;
        for attempt in 0..=self.max_retries {
            let fut = client
                .post(&url)
                .header(AUTHORIZATION, format!("Bearer {api_key}"))
                .json(&body)
                .send();

            let resp = match timeout(Duration::from_millis(self.timeout_ms), fut).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => {
                    last_err = Some(UnicError::Llm(format!("POST /responses failed: {e}")));
                    if attempt < self.max_retries {
                        sleep(Duration::from_millis(250_u64.saturating_mul(1 << attempt))).await;
                        continue;
                    }
                    return Err(last_err.unwrap());
                }
                Err(_) => {
                    last_err = Some(UnicError::Llm("POST /responses timeout".to_string()));
                    if attempt < self.max_retries {
                        sleep(Duration::from_millis(250_u64.saturating_mul(1 << attempt))).await;
                        continue;
                    }
                    return Err(last_err.unwrap());
                }
            };

            let status = resp.status();
            let text = resp
                .text()
                .await
                .map_err(|e| UnicError::Llm(format!("read /responses body failed: {e}")))?;

            if !status.is_success() {
                let msg = if text.len() > 2048 {
                    format!("{}…", &text[..2048])
                } else {
                    text
                };
                last_err = Some(UnicError::Llm(format!("provider http error {status}: {msg}")));
                let retryable = status.as_u16() == 429
                    || status.is_server_error()
                    || (status.as_u16() == 404
                        && msg.contains("No Codex account is available for responses relay"));
                if retryable && attempt < self.max_retries {
                    sleep(Duration::from_millis(250_u64.saturating_mul(1 << attempt))).await;
                    continue;
                }
                return Err(last_err.unwrap());
            }

            let response_json: Value = serde_json::from_str(&text)
                .map_err(|e| UnicError::Llm(format!("bad json: {e}")))?;
            let out =
                Self::extract_output_text(&response_json).unwrap_or_else(|| "".to_string());
            return Ok(out);
        }

        Err(last_err.unwrap_or_else(|| UnicError::Llm("unknown responses error".to_string())))
    }
}

#[async_trait]
impl LlmProvider for ResponsesProvider {
    async fn generate(&self, prompt: Vec<ChatMessage>) -> Result<String, UnicError> {
        let input = Self::prompt_to_input(prompt);
        self.responses_create(input).await
    }

    async fn should_respond(&self, context: &Perception) -> Result<bool, UnicError> {
        if context
            .recent
            .iter()
            .rev()
            .find(|m| matches!(m.sender, Sender::UserSudo))
            .is_none()
        {
            return Ok(false);
        }

        let input = Self::perception_to_gate_input(context);
        let out = self.responses_create(input).await?;
        let norm = out.trim().to_ascii_uppercase();
        if norm == "YES" || norm.starts_with("YES") {
            return Ok(true);
        }
        if norm == "NO" || norm.starts_with("NO") {
            return Ok(false);
        }
        if norm.contains("PONG") {
            return Ok(true);
        }
        Ok(norm.contains("YES") || norm.contains("TRUE") || norm.contains("RESPOND"))
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ResponsesCreateResponse {
    id: String,
}
