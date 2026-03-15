use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use unicos_bus::Bus;
use unicos_common::{
    AgentId, ChatMessage, ChatRole, Command, Envelope, Event, Message, Perception,
    Sender, Topic, UnicError,
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

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum NextAction {
    Speak { text: String },
    Tool {
        tool: String,
        #[serde(default)]
        args: Value,
    },
    Noop,
}

#[derive(Debug)]
pub struct ContextManager {
    window: usize,
    recent: VecDeque<Envelope<Message>>,
}

struct MailboxLogger {
    path: PathBuf,
    file: Option<tokio::fs::File>,
}

impl MailboxLogger {
    fn new(path: PathBuf) -> Self {
        Self { path, file: None }
    }

    async fn append(&mut self, env: &Envelope<Message>) -> Result<(), UnicError> {
        if self.file.is_none() {
            if let Some(parent) = self.path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .await?;
            self.file = Some(file);
        }

        let line = serde_json::to_string(env)?;
        let f = self.file.as_mut().expect("mailbox file must be open");
        f.write_all(line.as_bytes()).await?;
        f.write_all(b"\n").await?;
        Ok(())
    }
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

    fn tool_protocol_system_prompt() -> String {
        [
            "You are an autonomous agent running inside UnicOS.",
            "IMPORTANT: Tool calls and tool outputs are PRIVATE to you. Other participants will NOT see them.",
            "After using any tool, you MUST send a normal speak message that includes the relevant results (a short, user-facing summary and key output lines).",
            "You can think silently, but you MUST output ONLY a single JSON object as your final output.",
            "Valid actions:",
            r#"  - {"action":"speak","text":"..."}"#,
            r#"  - {"action":"tool","tool":"bash","args":{"cmd":"...","timeout_ms":30000}}"#,
            r#"  - {"action":"tool","tool":"fs","args":{"op":"read","path":"/path","max_bytes":65536}}"#,
            r#"  - {"action":"tool","tool":"fs","args":{"op":"write","path":"/path","data":"...","append":true}}"#,
            r#"  - {"action":"tool","tool":"net","args":{"method":"GET","url":"https://...","headers":{},"body":null}}"#,
            r#"  - {"action":"tool","tool":"conv_load","args":{"conversation_id":"<seed-or-8hex-id>","max_messages":200}}"#,
            r#"  - {"action":"noop"}"#,
            "Do not wrap JSON in Markdown fences. Do not include any extra keys.",
            "When you call a tool, its result will appear in the context as a tool(...) message; use it to decide the next action.",
            "If you need a tool, choose tool action; otherwise choose speak.",
        ]
        .join("\n")
    }

    fn strip_json_fence(s: &str) -> &str {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("```json") {
            return rest.trim().trim_end_matches("```").trim();
        }
        if let Some(rest) = s.strip_prefix("```") {
            return rest.trim().trim_end_matches("```").trim();
        }
        s
    }

    fn parse_next_action(raw: &str) -> Option<NextAction> {
        let raw = Self::strip_json_fence(raw);
        serde_json::from_str::<NextAction>(raw).ok()
    }

    pub async fn decide_with_trace(
        &self,
        perception: &Perception,
        trace: &[ChatMessage],
    ) -> Result<Option<AgentAction>, UnicError> {
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
        if let Some(conv) = last.strip_prefix("!conv_load ") {
            return Ok(Some(AgentAction::ToolCall {
                tool: "conv_load".to_string(),
                args: serde_json::json!({ "conversation_id": conv, "max_messages": 200 }),
            }));
        }

        if trace.is_empty() && !self.provider.should_respond(perception).await? {
            return Ok(None);
        }

        let mut prompt = Vec::new();
        prompt.push(ChatMessage {
            role: ChatRole::System,
            content: Self::tool_protocol_system_prompt(),
            name: None,
        });
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
        prompt.extend_from_slice(trace);
        prompt.push(ChatMessage {
            role: ChatRole::User,
            content: "Return the next action JSON now.".to_string(),
            name: None,
        });

        let text = self.provider.generate(prompt).await?;
        if let Some(action) = Self::parse_next_action(&text) {
            return Ok(match action {
                NextAction::Speak { text } => Some(AgentAction::Speak(text)),
                NextAction::Tool { tool, args } => {
                    let args = if args.is_null() { serde_json::json!({}) } else { args };
                    Some(AgentAction::ToolCall { tool, args })
                }
                NextAction::Noop => None,
            });
        }
        Ok(Some(AgentAction::Speak(text)))
    }

    pub async fn decide(&self, perception: &Perception) -> Result<Option<AgentAction>, UnicError> {
        self.decide_with_trace(perception, &[]).await
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
    mailbox: MailboxLogger,
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
        let mailbox = MailboxLogger::new(cfg.soul_dir.join("mailbox.log"));
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
            mailbox,
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
                    let Envelope { id, ts, topic, sender, conversation_id, payload } = env;
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

                            let env_for_mailbox = Envelope {
                                id,
                                ts,
                                topic: topic.clone(),
                                sender: sender.clone(),
                                conversation_id: conversation_id.clone(),
                                payload: msg.clone(),
                            };
                            if let Err(e) = self.mailbox.append(&env_for_mailbox).await {
                                warn!("agent {} mailbox append failed: {e}", self.cfg.id);
                            }

                            self.ctx.push_message(Envelope {
                                id,
                                ts,
                                topic: topic.clone(),
                                sender,
                                conversation_id: conversation_id.clone(),
                                payload: msg,
                            });
                            let soul = Self::read_soul(&self.cfg.soul_dir).await;
                            let perception = Perception{
                                agent_id: self.cfg.id.clone(),
                                topic: topic.clone(),
                                soul,
                                recent: self.ctx.snapshot(),
                            };

                            let mut trace: Vec<ChatMessage> = Vec::new();
                            let mut steps = 0usize;
                            let max_steps = 6usize;
                            loop {
                                if steps >= max_steps {
                                    self.bus.publish(self.bus.envelope_with_conversation(
                                        topic.clone(),
                                        Sender::Agent(self.cfg.id.clone()),
                                        conversation_id.clone(),
                                        Event::Message(Message {
                                            text: "工具调用回合数超限，已停止（请缩小任务或指定更明确的命令）。"
                                                .to_string(),
                                        }),
                                    ))?;
                                    break;
                                }

                                let Some(action) = self.decision.decide_with_trace(&perception, &trace).await? else {
                                    break;
                                };
                                match action {
                                    AgentAction::Speak(text) => {
                                        self.bus.publish(self.bus.envelope_with_conversation(
                                            topic.clone(),
                                            Sender::Agent(self.cfg.id.clone()),
                                            conversation_id.clone(),
                                            Event::Message(Message { text }),
                                        ))?;
                                        break;
                                    }
                                    AgentAction::ToolCall { tool, args } => {
                                        steps += 1;

                                        if !self.cfg.tools_enabled {
                                            trace.push(ChatMessage {
                                                role: ChatRole::Tool,
                                                content: "ERROR: tools disabled".to_string(),
                                                name: Some(tool.clone()),
                                            });
                                            continue;
                                        }

                                        let tool_result = self.tools.call(&tool, args).await;
                                        let trace_msg = match tool_result {
                                            Ok(v) => {
                                                let s = serde_json::to_string(&v).unwrap_or_else(|_| "\"<non-json>\"".to_string());
                                                ChatMessage { role: ChatRole::Tool, content: s, name: Some(tool.clone()) }
                                            }
                                            Err(e) => {
                                                ChatMessage { role: ChatRole::Tool, content: format!("ERROR: {e}"), name: Some(tool.clone()) }
                                            }
                                        };

                                        trace.push(trace_msg);
                                        continue;
                                    }
                                }
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
    use unicos_common::ConversationId;
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
                conversation_id: ConversationId::default(),
                payload: Message {
                    text: "are you there?".to_string(),
                },
            }],
        };
        let action = engine.decide(&perception).await.unwrap();
        assert!(matches!(action, Some(AgentAction::Speak(_))));
    }

    #[tokio::test]
    async fn mailbox_appends_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mailbox.log");
        let mut m = MailboxLogger::new(path.clone());
        let env = Envelope {
            id: uuid::Uuid::now_v7(),
            ts: chrono::Utc::now(),
            topic: Topic::new("general"),
            sender: Sender::UserSudo,
            conversation_id: ConversationId::default(),
            payload: Message {
                text: "hello".to_string(),
            },
        };
        m.append(&env).await.unwrap();
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("\"hello\""));
        assert!(content.lines().count() >= 1);
    }
}
