use serde::Deserialize;
use std::path::PathBuf;
use unicos_common::{ChatMessage, ChatRole};
use unicos_llm::{LlmProvider, ResponsesProvider};

#[derive(Debug, Deserialize)]
struct ChobitsConfig {
    model: ModelSection,
}

#[derive(Debug, Deserialize)]
struct ModelSection {
    #[allow(dead_code)]
    provider: Option<String>,
    model: Option<String>,
    responses: Option<ModelResponses>,
}

#[derive(Debug, Deserialize)]
struct ModelResponses {
    base_url: String,
    api_key_env: String,
    api_key: Option<String>,
}

fn default_chobits_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/midori".to_string());
    PathBuf::from(home).join("chobits/config.toml")
}

fn normalize(s: &str) -> String {
    s.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_chobits_config_path);

    let raw = tokio::fs::read_to_string(&path).await?;
    let cfg: ChobitsConfig = toml::from_str(&raw)?;
    let model = cfg.model.model.unwrap_or_else(|| "gpt-5.2".to_string());
    let responses = cfg
        .model
        .responses
        .ok_or("missing [model.responses] in config")?;

    let mut provider = ResponsesProvider::new(
        responses.base_url.clone(),
        responses.api_key_env.clone(),
        responses.api_key.clone(),
        model.clone(),
    );
    provider.max_retries = 8;
    provider.timeout_ms = 60_000;

    eprintln!(
        "Ping/Pong: model={} base_url={} api_key_env={}",
        model, responses.base_url, responses.api_key_env
    );

    let out = provider
        .generate(vec![
            ChatMessage {
                role: ChatRole::System,
                content: "You are a connectivity test. Reply exactly with Pong and nothing else."
                    .to_string(),
                name: None,
            },
            ChatMessage {
                role: ChatRole::User,
                content: "Ping".to_string(),
                name: None,
            },
        ])
        .await?;

    let norm = normalize(&out);
    if norm == "Pong" {
        println!("OK: Pong");
        return Ok(());
    }
    if norm.to_lowercase().starts_with("pong") {
        println!("WARN: got {:?} (starts with Pong)", norm);
        return Ok(());
    }

    Err(format!("FAIL: expected Pong, got {:?}", norm).into())
}

