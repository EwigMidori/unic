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

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Topic {
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
    JoinTopic { id: AgentId, topic: Topic },
    Sleep { id: AgentId },
    Wake { id: AgentId },
    SetUserTopic { topic: Topic },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SystemNotification {
    AgentSpawned { id: AgentId },
    AgentKilled { id: AgentId },
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
    pub payload: T,
}

impl<T> Envelope<T> {
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Envelope<U> {
        Envelope {
            id: self.id,
            ts: self.ts,
            topic: self.topic,
            sender: self.sender,
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
