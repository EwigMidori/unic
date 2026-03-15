use crate::{Command, Envelope, Event, Sender, Topic};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    Ping,
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
    Error { message: String },
    Event { envelope: Envelope<Event> },
}

