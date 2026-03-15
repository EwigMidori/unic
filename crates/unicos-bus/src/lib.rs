use chrono::Utc;
use tokio::sync::broadcast;
use unicos_common::{Envelope, Event, Sender, Topic, UnicError};
use uuid::Uuid;

#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Envelope<Event>>,
}

impl Bus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Envelope<Event>> {
        self.tx.subscribe()
    }

    pub fn publish(&self, envelope: Envelope<Event>) -> Result<(), UnicError> {
        self.tx.send(envelope).map(|_| ()).map_err(|_| UnicError::BusSend)
    }

    pub fn envelope(&self, topic: Topic, sender: Sender, payload: Event) -> Envelope<Event> {
        Envelope {
            id: Uuid::now_v7(),
            ts: Utc::now(),
            topic,
            sender,
            payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicos_common::Message;

    #[tokio::test]
    async fn publish_and_receive() {
        let bus = Bus::new(16);
        let mut rx = bus.subscribe();
        bus.publish(bus.envelope(
            Topic::new("general"),
            Sender::UserSudo,
            Event::Message(Message {
                text: "hi".to_string(),
            }),
        ))
        .unwrap();

        let env = rx.recv().await.unwrap();
        assert_eq!(env.topic, Topic::new("general"));
        assert!(matches!(env.sender, Sender::UserSudo));
        match env.payload {
            Event::Message(m) => assert_eq!(m.text, "hi"),
            _ => panic!("unexpected event"),
        }
    }
}
