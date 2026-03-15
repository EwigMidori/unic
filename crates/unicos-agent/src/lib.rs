use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use unicos_bus::Bus;
use unicos_common::{
    AgentId, ChatMessage, ChatRole, Command, Envelope, Event, Message, Perception, Sender, Topic,
    UnicError,
};
use unicos_llm::LlmProvider;
use unicos_tools::ToolRegistry;

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub id: AgentId,
    pub topics: Vec<Topic>,
    pub perception_window: usize,
    pub soul_dir: PathBuf,
    pub tools_enabled: bool,
}

#[derive(Debug)]
pub enum AgentAction {
    Speak(String),
    ToolCall { tool: String, args: Value },
}

#[derive(Debug)]
pub struct ContextManager {
    window: usize,
    recent: VecDeque<Envelope<Message>>,
}

impl ContextManager {
    pub fn new(window: usize) -> Self {
        Self {
            window: window.max(1),
            recent: VecDeque::new(),
        }
    }

    pub fn push_message(&mut self, msg: Envelope<Message>) {
        self.recent.push_back(msg);
        while self.recent.len() > self.window {
            self.recent.pop_front();
        }
    }

    pub fn snapshot(&self) -> Vec<Envelope<Message>> {
        self.recent.iter().cloned().collect()
    }
}

pub struct DecisionEngine {
    provider: Arc<dyn LlmProvider>,
}

impl DecisionEngine {
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self { provider }
    }

    pub async fn decide(&self, perception: &Perception) -> Result<Option<AgentAction>, UnicError> {
        if !self.provider.should_respond(perception).await? {
            return Ok(None);
        }

        let last = perception.recent.last().map(|m| m.payload.text.clone()).unwrap_or_default();
        if let Some(cmd) = last.strip_prefix("!bash ") {
            return Ok(Some(AgentAction::ToolCall {
                tool: "bash".to_string(),
                args: serde_json::json!({ "cmd": cmd, "timeout_ms": 30000 }),
            }));
        }
        if let Some(cmd) = last.strip_prefix("!fs_read ") {
            return Ok(Some(AgentAction::ToolCall {
                tool: "fs".to_string(),
                args: serde_json::json!({ "op": "read", "path": cmd, "max_bytes": 65536 }),
            }));
        }
        if let Some(url) = last.strip_prefix("!net_get ") {
            return Ok(Some(AgentAction::ToolCall {
                tool: "net".to_string(),
                args: serde_json::json!({ "method": "GET", "url": url }),
            }));
        }

        let mut prompt = Vec::new();
        if !perception.soul.trim().is_empty() {
            prompt.push(ChatMessage {
                role: ChatRole::System,
                content: perception.soul.clone(),
                name: None,
            });
        }
        for m in &perception.recent {
            prompt.push(ChatMessage {
                role: match m.sender {
                    Sender::UserSudo => ChatRole::User,
                    Sender::Agent(_) => ChatRole::Assistant,
                    Sender::System => ChatRole::System,
                },
                content: format!("{} {}", m.sender.label(), m.payload.text),
                name: None,
            });
        }

        let text = self.provider.generate(prompt).await?;
        Ok(Some(AgentAction::Speak(text)))
    }
}

pub struct AgentRuntime {
    cfg: AgentConfig,
    bus: Bus,
    rx: broadcast::Receiver<Envelope<Event>>,
    tools: Arc<ToolRegistry>,
    decision: DecisionEngine,
    cancel: CancellationToken,
    sleeping: bool,
    subscribed: HashSet<Topic>,
    ctx: ContextManager,
}

impl AgentRuntime {
    pub fn new(
        cfg: AgentConfig,
        bus: Bus,
        provider: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        cancel: CancellationToken,
    ) -> Self {
        let mut subscribed = HashSet::new();
        for t in &cfg.topics {
            subscribed.insert(t.clone());
        }
        Self {
            rx: bus.subscribe(),
            decision: DecisionEngine::new(provider),
            ctx: ContextManager::new(cfg.perception_window),
            cfg,
            bus,
            tools,
            cancel,
            sleeping: false,
            subscribed,
        }
    }

    async fn read_soul(soul_dir: &Path) -> String {
        let p = soul_dir.join("soul.md");
        match tokio::fs::read_to_string(p).await {
            Ok(s) => s,
            Err(_) => String::new(),
        }
    }

    fn should_accept(&self, topic: &Topic) -> bool {
        self.subscribed.contains(topic)
    }

    async fn handle_command(&mut self, cmd: Command) -> Result<(), UnicError> {
        match cmd {
            Command::JoinTopic { id, topic } if id == self.cfg.id => {
                self.subscribed.insert(topic.clone());
                self.bus.publish(self.bus.envelope(
                    topic.clone(),
                    Sender::System,
                    Event::SystemNotification(unicos_common::SystemNotification::AgentJoinedTopic {
                        id: self.cfg.id.clone(),
                        topic,
                    }),
                ))?;
            }
            Command::Sleep { id } if id == self.cfg.id => {
                self.sleeping = true;
                self.bus.publish(self.bus.envelope(
                    Topic::new("system"),
                    Sender::System,
                    Event::SystemNotification(unicos_common::SystemNotification::AgentSlept {
                        id: self.cfg.id.clone(),
                    }),
                ))?;
            }
            Command::Wake { id } if id == self.cfg.id => {
                self.sleeping = false;
                self.bus.publish(self.bus.envelope(
                    Topic::new("system"),
                    Sender::System,
                    Event::SystemNotification(unicos_common::SystemNotification::AgentWoke {
                        id: self.cfg.id.clone(),
                    }),
                ))?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn act(&self, topic: Topic, action: AgentAction) -> Result<(), UnicError> {
        match action {
            AgentAction::Speak(text) => {
                self.bus.publish(self.bus.envelope(
                    topic,
                    Sender::Agent(self.cfg.id.clone()),
                    Event::Message(Message { text }),
                ))?;
            }
            AgentAction::ToolCall { tool, args } => {
                if !self.cfg.tools_enabled {
                    self.bus.publish(self.bus.envelope(
                        topic,
                        Sender::Agent(self.cfg.id.clone()),
                        Event::Message(Message {
                            text: format!("工具已禁用，跳过调用：{tool}"),
                        }),
                    ))?;
                    return Ok(());
                }
                let result = self.tools.call(&tool, args).await?;
                self.bus.publish(self.bus.envelope(
                    topic,
                    Sender::Agent(self.cfg.id.clone()),
                    Event::Message(Message {
                        text: format!("tool:{tool} -> {}", result),
                    }),
                ))?;
            }
        }
        Ok(())
    }

    pub async fn run(mut self) -> Result<(), UnicError> {
        info!("agent {} started", self.cfg.id);
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("agent {} stopping", self.cfg.id);
                    return Ok(());
                }
                recv = self.rx.recv() => {
                    let env = match recv {
                        Ok(v) => v,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("agent {} lagged {n} messages", self.cfg.id);
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    };
                    let Envelope { id, ts, topic, sender, payload } = env;
                    match payload {
                        Event::Command(cmd) => {
                            self.handle_command(cmd).await?;
                            continue;
                        }
                        Event::SystemNotification(_) => continue,
                        Event::Message(msg) => {
                            if !self.should_accept(&topic) {
                                continue;
                            }
                            if matches!(sender, Sender::Agent(ref sid) if sid == &self.cfg.id) {
                                continue;
                            }
                            if self.sleeping {
                                continue;
                            }

                            self.ctx.push_message(Envelope {
                                id,
                                ts,
                                topic: topic.clone(),
                                sender,
                                payload: msg,
                            });
                            let soul = Self::read_soul(&self.cfg.soul_dir).await;
                            let perception = Perception{
                                agent_id: self.cfg.id.clone(),
                                topic: topic.clone(),
                                soul,
                                recent: self.ctx.snapshot(),
                            };

                            if let Some(action) = self.decision.decide(&perception).await? {
                                self.act(topic.clone(), action).await?;
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicos_llm::RuleBasedProvider;

    #[tokio::test]
    async fn decision_engine_triggers_on_question() {
        let provider: Arc<dyn LlmProvider> = Arc::new(RuleBasedProvider::default());
        let engine = DecisionEngine::new(provider);
        let perception = Perception {
            agent_id: AgentId::new("Alice"),
            topic: Topic::new("general"),
            soul: String::new(),
            recent: vec![Envelope {
                id: uuid::Uuid::now_v7(),
                ts: chrono::Utc::now(),
                topic: Topic::new("general"),
                sender: Sender::UserSudo,
                payload: Message {
                    text: "are you there?".to_string(),
                },
            }],
        };
        let action = engine.decide(&perception).await.unwrap();
        assert!(matches!(action, Some(AgentAction::Speak(_))));
    }
}
