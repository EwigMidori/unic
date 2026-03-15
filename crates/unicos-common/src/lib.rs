use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

pub mod ipc;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(String);

impl AgentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Topic(String);

impl Topic {
    pub fn new(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        if raw.starts_with('#') {
            Self(raw)
        } else {
            Self(format!("#{raw}"))
        }
    }

    pub fn dm(a: impl AsRef<str>, b: impl AsRef<str>) -> Self {
        let a = a.as_ref().trim();
        let b = b.as_ref().trim();
        let (x, y) = if a <= b { (a, b) } else { (b, a) };
        Topic::new(format!("dm:{x}:{y}"))
    }

    pub fn is_dm(&self) -> bool {
        self.0.starts_with("#dm:")
    }

    pub fn dm_participants(&self) -> Option<(&str, &str)> {
        if !self.is_dm() {
            return None;
        }
        let rest = self.0.strip_prefix("#dm:")?;
        let mut it = rest.splitn(3, ':');
        let a = it.next()?;
        let b = it.next()?;
        if a.is_empty() || b.is_empty() {
            return None;
        }
        Some((a, b))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationId(String);

impl ConversationId {
    pub fn from_seed(seed: impl AsRef<str>) -> Self {
        let seed = seed.as_ref().trim();
        let seed = if seed.is_empty() { "default" } else { seed };
        let input = format!("unicos:conversation:{seed}");
        let hash = blake3::hash(input.as_bytes());
        Self(hash.to_hex().to_string())
    }

    pub fn parse_hex(raw: impl AsRef<str>) -> Option<Self> {
        let raw = raw.as_ref().trim();
        if raw.len() != 64 {
            return None;
        }
        if !raw.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(Self(raw.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ConversationId {
    fn default() -> Self {
        Self::from_seed("default")
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sender {
    UserSudo,
    Agent(AgentId),
    System,
}

impl Default for Sender {
    fn default() -> Self {
        Sender::UserSudo
    }
}

impl Sender {
    pub fn label(&self) -> String {
        match self {
            Sender::UserSudo => "[User: sudo]".to_string(),
            Sender::Agent(id) => format!("[Unic: {id}]"),
            Sender::System => "[System]".to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    Spawn { id: AgentId },
    Kill { id: AgentId },
    Purge { id: AgentId, confirm: bool },
    JoinTopic { id: AgentId, topic: Topic },
    Sleep { id: AgentId },
    Wake { id: AgentId },
    SetUserTopic { topic: Topic },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SystemNotification {
    AgentSpawned { id: AgentId },
    AgentKilled { id: AgentId },
    AgentPurged { id: AgentId },
    AgentPanicked { id: AgentId, message: String },
    AgentJoinedTopic { id: AgentId, topic: Topic },
    AgentSlept { id: AgentId },
    AgentWoke { id: AgentId },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    Message(Message),
    Command(Command),
    SystemNotification(SystemNotification),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub id: Uuid,
    pub ts: DateTime<Utc>,
    pub topic: Topic,
    pub sender: Sender,
    #[serde(default)]
    pub conversation_id: ConversationId,
    pub payload: T,
}

impl<T> Envelope<T> {
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Envelope<U> {
        Envelope {
            id: self.id,
            ts: self.ts,
            topic: self.topic,
            sender: self.sender,
            conversation_id: self.conversation_id,
            payload: f(self.payload),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    pub name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Perception {
    pub agent_id: AgentId,
    pub topic: Topic,
    pub soul: String,
    pub recent: Vec<Envelope<Message>>,
}

#[derive(Debug, thiserror::Error)]
pub enum UnicError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde json error: {0}")]
    SerdeJson(#[from] serde_json::Error),

    #[error("toml deserialize error: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("toml serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("bus send error")]
    BusSend,

    #[error("llm error: {0}")]
    Llm(String),

    #[error("tool error: {0}")]
    Tool(String),

    #[error("notify error: {0}")]
    Notify(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_topic_is_normalized() {
        assert_eq!(
            Topic::dm("bob", "alice").as_str(),
            "#dm:alice:bob"
        );
        assert_eq!(
            Topic::dm("sudo", "Pinger").as_str(),
            "#dm:Pinger:sudo"
        );
    }

    #[test]
    fn dm_participants_parses() {
        let t = Topic::new("#dm:alice:bob");
        assert!(t.is_dm());
        assert_eq!(t.dm_participants(), Some(("alice", "bob")));
        let bad = Topic::new("#dm:alice");
        assert_eq!(bad.dm_participants(), None);
    }

    #[test]
    fn conversation_id_is_stable_hex() {
        let a = ConversationId::from_seed("task-1");
        let b = ConversationId::from_seed("task-1");
        assert_eq!(a, b);
        assert_eq!(a.as_str().len(), 64);
        assert!(ConversationId::parse_hex(a.as_str()).is_some());
    }
}
