mod responses;

use async_trait::async_trait;
pub use responses::ResponsesProvider;
use unicos_common::{ChatMessage, ChatRole, Perception, UnicError};

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn generate(&self, prompt: Vec<ChatMessage>) -> Result<String, UnicError>;
    async fn should_respond(&self, context: &Perception) -> Result<bool, UnicError>;
}

#[derive(Debug, Default)]
pub struct RuleBasedProvider;

impl RuleBasedProvider {
    fn mentioned(agent: &str, text: &str) -> bool {
        text.contains(&format!("@{agent}"))
    }

    fn last_user_line(perception: &Perception) -> Option<String> {
        perception
            .recent
            .iter()
            .rev()
            .find(|m| matches!(m.sender, unicos_common::Sender::UserSudo))
            .map(|m| m.payload.text.clone())
    }
}

#[async_trait]
impl LlmProvider for RuleBasedProvider {
    async fn generate(&self, prompt: Vec<ChatMessage>) -> Result<String, UnicError> {
        let last = prompt.iter().rev().find(|m| matches!(m.role, ChatRole::User));
        let Some(last) = last else {
            return Ok("……".to_string());
        };
        Ok(format!("收到：{}", last.content))
    }

    async fn should_respond(&self, context: &Perception) -> Result<bool, UnicError> {
        let Some(last_user) = Self::last_user_line(context) else {
            return Ok(false);
        };
        let id = context.agent_id.as_str();
        let should = last_user.contains('?')
            || last_user.contains('？')
            || Self::mentioned(id, &last_user)
            || last_user.to_lowercase().contains("help");
        Ok(should)
    }
}
