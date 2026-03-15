use crate::{Command, Envelope, Event, Sender, Topic};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    Ping,
    ListAgents,
    Publish {
        topic: Topic,
        text: String,
        #[serde(default)]
        sender: Sender,
    },
    Command { cmd: Command },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Pong,
    Agents { ids: Vec<crate::AgentId> },
    Error { message: String },
    Event { envelope: Envelope<Event> },
}
