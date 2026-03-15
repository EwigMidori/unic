use reedline::{
    default_emacs_keybindings, ColumnarMenu, Completer, EditCommand, Emacs, ExternalPrinter,
    Highlighter, KeyCode, KeyModifiers, Keybindings, Prompt, PromptEditMode, PromptHistorySearch,
    MenuBuilder, Reedline, ReedlineEvent, ReedlineMenu, Signal, Span, StyledText, Suggestion,
};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::warn;
use unicos_common::ipc::{ClientRequest, ServerMessage};
use unicos_common::{AgentId, Command, Envelope, Event, Message, Sender, Topic};

fn parse_args() -> String {
    let mut socket = "/tmp/unicos.sock".to_string();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => {
                if let Some(p) = args.next() {
                    socket = p;
                }
            }
            _ => {}
        }
    }
    socket
}

fn user_dm_id() -> &'static str {
    "sudo"
}

#[derive(Default)]
struct CompletionState {
    active_agents: BTreeSet<String>,
    known_agents: BTreeSet<String>,
    topics: BTreeSet<String>,
}

impl CompletionState {
    fn note_topic(&mut self, topic: &Topic) {
        self.topics.insert(topic.to_string());
    }

    fn note_agent_spawned(&mut self, id: &AgentId) {
        self.active_agents.insert(id.to_string());
        self.known_agents.insert(id.to_string());
    }

    fn note_agent_killed(&mut self, id: &AgentId) {
        self.active_agents.remove(id.as_str());
        self.known_agents.insert(id.to_string());
    }

    fn note_agent_purged(&mut self, id: &AgentId) {
        self.active_agents.remove(id.as_str());
        self.known_agents.remove(id.as_str());
    }

    fn agent_suggestions(&self) -> Vec<String> {
        let mut out = self.active_agents.iter().cloned().collect::<Vec<_>>();
        for id in &self.known_agents {
            if !self.active_agents.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }

    fn topic_suggestions(&self) -> Vec<String> {
        let mut out = vec!["#general".to_string(), "#system".to_string(), "#project_x".to_string()];
        for t in &self.topics {
            if !out.contains(t) {
                out.push(t.clone());
            }
        }
        out
    }
}

fn format_event(env: Envelope<Event>) -> Option<String> {
    match env.payload {
        Event::Message(Message { text }) => Some(format!(
            "{} {} {} {}",
            env.ts,
            env.topic,
            env.sender.label(),
            text
        )),
        Event::SystemNotification(n) => Some(format!("{} {} [SystemNotice] {:?}", env.ts, env.topic, n)),
        Event::Command(_) => None,
    }
}

#[derive(Clone)]
struct UnicosPrompt {
    topic: Arc<Mutex<String>>,
}

impl Prompt for UnicosPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        let t = self.topic.lock().unwrap();
        Cow::Owned(format!("{t} "))
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("... ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        Cow::Borrowed("search> ")
    }
}

#[derive(Default)]
struct UnicosHighlighter;

impl Highlighter for UnicosHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut out = StyledText::new();
        let default_style = nu_ansi_term::Style::default();
        out.push((default_style, line.to_string()));

        let cmd_style = nu_ansi_term::Style::new()
            .fg(nu_ansi_term::Color::Cyan)
            .bold();
        let topic_style = nu_ansi_term::Style::new().fg(nu_ansi_term::Color::LightBlue);
        let bang_style = nu_ansi_term::Style::new().fg(nu_ansi_term::Color::Yellow);
        let dm_style = nu_ansi_term::Style::new().fg(nu_ansi_term::Color::Purple);

        let trimmed = line.trim_start();
        if trimmed.starts_with('/') {
            let leading_ws = line.len() - trimmed.len();
            let end = trimmed
                .find(char::is_whitespace)
                .map(|i| leading_ws + i)
                .unwrap_or(line.len());
            out.style_range(leading_ws, end, cmd_style);
        }
        if trimmed.starts_with('!') {
            let leading_ws = line.len() - trimmed.len();
            let end = trimmed
                .find(char::is_whitespace)
                .map(|i| leading_ws + i)
                .unwrap_or(line.len());
            out.style_range(leading_ws, end, bang_style);
        }
        if trimmed.starts_with('@') {
            let leading_ws = line.len() - trimmed.len();
            let end = trimmed
                .find(char::is_whitespace)
                .map(|i| leading_ws + i)
                .unwrap_or(line.len());
            out.style_range(leading_ws, end, dm_style);
        }

        for (idx, _) in line.match_indices('#') {
            let end = line[idx..]
                .find(|c: char| c.is_whitespace())
                .map(|i| idx + i)
                .unwrap_or(line.len());
            if end > idx + 1 {
                out.style_range(idx, end, topic_style);
            }
        }

        out
    }
}

struct UnicosCompleter {
    commands: Vec<(&'static str, &'static str)>,
    state: Arc<Mutex<CompletionState>>,
}

impl UnicosCompleter {
    fn new(state: Arc<Mutex<CompletionState>>) -> Self {
        Self {
            commands: vec![
                ("/ping", "daemon connectivity"),
                ("/agents", "list running agents"),
                ("/topic", "switch current topic"),
                ("/dm", "switch to dm with agent"),
                ("/spawn", "spawn agent"),
                ("/kill", "kill agent"),
                ("/purge", "delete agent on disk (needs confirm)"),
                ("/join", "agent join topic"),
                ("/sleep", "sleep agent"),
                ("/wake", "wake agent"),
                ("/quit", "exit cli"),
                ("/exit", "exit cli"),
            ],
            state,
        }
    }
}

impl UnicosCompleter {
    fn word_span(line: &str, pos: usize) -> (usize, usize, &str) {
        let pos = pos.min(line.len());
        let left = line[..pos]
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        let right = line[pos..]
            .find(char::is_whitespace)
            .map(|i| pos + i)
            .unwrap_or(line.len());
        (left, right, &line[left..pos])
    }

    fn tokenize(line: &str) -> Vec<&str> {
        line.split_whitespace().collect()
    }
}

impl Completer for UnicosCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let trimmed = line.trim_start();
        if trimmed.starts_with('@') {
            let (start, end, prefix) = Self::word_span(line, pos);
            let span = Span::new(start, end);
            let prefix = prefix.strip_prefix('@').unwrap_or(prefix);
            let agents = self.state.lock().unwrap().agent_suggestions();
            return agents
                .into_iter()
                .filter(|a| a.starts_with(prefix))
                .map(|a| Suggestion {
                    value: format!("@{a}"),
                    description: Some("dm".to_string()),
                    span,
                    append_whitespace: true,
                    ..Suggestion::default()
                })
                .collect();
        }

        let (start, end, prefix) = Self::word_span(line, pos);
        let tokens = Self::tokenize(&line[..pos]);
        let span = Span::new(start, end);
        let mut out = Vec::new();

        let is_cmd_line = line.trim_start().starts_with('/');
        if is_cmd_line {
            let completing_cmd = tokens.len() <= 1;
            if completing_cmd {
                for (cmd, desc) in &self.commands {
                    if cmd.starts_with(prefix) {
                        out.push(Suggestion {
                            value: (*cmd).to_string(),
                            description: Some((*desc).to_string()),
                            span,
                            append_whitespace: true,
                            ..Suggestion::default()
                        });
                    }
                }
                return out;
            }

            let cmd = tokens.first().copied().unwrap_or("");
            let arg_index = tokens.len().saturating_sub(1);
            let completing_agent = matches!(cmd, "/kill" | "/sleep" | "/wake" | "/purge" | "/dm" | "/join") && arg_index == 1;
            if completing_agent {
                let agents = self.state.lock().unwrap().agent_suggestions();
                for a in agents {
                    if a.starts_with(prefix) || prefix.is_empty() {
                        out.push(Suggestion {
                            value: a,
                            description: Some("agent".to_string()),
                            span,
                            append_whitespace: true,
                            ..Suggestion::default()
                        });
                    }
                }
                return out;
            }

            let completing_topic =
                (matches!(cmd, "/topic") && arg_index == 1) || (matches!(cmd, "/join") && arg_index == 2);
            if completing_topic {
                let topics = self.state.lock().unwrap().topic_suggestions();
                for t in topics {
                    if t.starts_with(prefix) || prefix.is_empty() {
                        out.push(Suggestion {
                            value: t,
                            description: Some("topic".to_string()),
                            span,
                            append_whitespace: true,
                            ..Suggestion::default()
                        });
                    }
                }
                return out;
            }
        }

        out
    }
}

fn add_completion_keybindings(keybindings: &mut Keybindings) {
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let socket = parse_args();
    let stream = UnixStream::connect(&socket).await?;
    let (read_half, mut write_half) = stream.into_split();

    let current_topic = Arc::new(Mutex::new(Topic::new("general")));
    let prompt_topic = Arc::new(Mutex::new("#general".to_string()));
    let completion_state = Arc::new(Mutex::new(CompletionState::default()));

    let printer = ExternalPrinter::default();
    let printer_for_server = printer.clone();
    let printer_for_cli = printer.clone();

    let completion_state_for_server = Arc::clone(&completion_state);
    tokio::spawn(async move {
        let mut lines = BufReader::new(read_half).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg = match serde_json::from_str::<ServerMessage>(&line) {
                Ok(m) => m,
                Err(e) => {
                    let _ = printer_for_server.print(format!("bad server message: {e}: {line}"));
                    continue;
                }
            };
            match msg {
                ServerMessage::Event { envelope } => {
                    {
                        let mut st = completion_state_for_server.lock().unwrap();
                        st.note_topic(&envelope.topic);
                        if let Event::SystemNotification(ref n) = envelope.payload {
                            match n {
                                unicos_common::SystemNotification::AgentSpawned { id } => st.note_agent_spawned(id),
                                unicos_common::SystemNotification::AgentKilled { id } => st.note_agent_killed(id),
                                unicos_common::SystemNotification::AgentPurged { id } => st.note_agent_purged(id),
                                _ => {}
                            }
                        }
                    }
                    if let Some(s) = format_event(envelope) {
                        let _ = printer_for_server.print(s);
                    }
                }
                ServerMessage::Pong => {
                    let _ = printer_for_server.print("Pong".to_string());
                }
                ServerMessage::Agents { ids } => {
                    let list = ids
                        .into_iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = printer_for_server.print(format!("agents: {list}"));
                }
                ServerMessage::Error { message } => {
                    let _ = printer_for_server.print(format!("[daemon error] {message}"));
                }
            }
        }
    });

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<String>();
    let prompt = UnicosPrompt {
        topic: Arc::clone(&prompt_topic),
    };

    let completion_state_for_editor = Arc::clone(&completion_state);
    tokio::task::spawn_blocking(move || {
        let mut keybindings = default_emacs_keybindings();
        add_completion_keybindings(&mut keybindings);
        let edit_mode = Box::new(Emacs::new(keybindings));

        let completion_menu = Box::new(
            ColumnarMenu::default()
                .with_name("completion_menu")
                .with_columns(4)
                .with_column_padding(2),
        );

        let mut editor = Reedline::create()
            .with_external_printer(printer)
            .with_highlighter(Box::new(UnicosHighlighter::default()))
            .with_completer(Box::new(UnicosCompleter::new(completion_state_for_editor)))
            .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
            .with_edit_mode(edit_mode);

        loop {
            match editor.read_line(&prompt) {
                Ok(Signal::Success(buffer)) => {
                    let line = buffer.trim().to_string();
                    if line == "/quit" || line == "/exit" {
                        break;
                    }
                    if !line.is_empty() && input_tx.send(line).is_err() {
                        break;
                    }
                }
                Ok(Signal::CtrlD) | Ok(Signal::CtrlC) => break,
                Err(_) => break,
            }
        }
    });

    warn!("Connected to {socket}. Use /spawn /kill /join /sleep /wake /topic; plain text sends to current topic.");

    while let Some(line) = input_rx.recv().await {
        if let Some(rest) = line.strip_prefix('@') {
            let mut it = rest.splitn(2, char::is_whitespace);
            let peer = it.next().unwrap_or("").trim();
            let msg = it.next().unwrap_or("").trim();
            if !peer.is_empty() && !msg.is_empty() {
                let dm_topic = Topic::dm(user_dm_id(), peer);
                let join = ClientRequest::Command {
                    cmd: Command::JoinTopic {
                        id: AgentId::new(peer),
                        topic: dm_topic.clone(),
                    },
                };
                write_half
                    .write_all(serde_json::to_string(&join)?.as_bytes())
                    .await?;
                write_half.write_all(b"\n").await?;

                let req = ClientRequest::Publish {
                    topic: dm_topic,
                    text: msg.to_string(),
                    sender: Sender::UserSudo,
                };
                write_half
                    .write_all(serde_json::to_string(&req)?.as_bytes())
                    .await?;
                write_half.write_all(b"\n").await?;
                continue;
            }
            let _ = printer_for_cli.print("usage: @<AgentId> <text>".to_string());
            continue;
        }

        if let Some(rest) = line.strip_prefix('/') {
            let parts = rest.split_whitespace().collect::<Vec<_>>();
            match parts.as_slice() {
                ["ping"] => {
                    let req = ClientRequest::Ping;
                    write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
                    write_half.write_all(b"\n").await?;
                }
                ["agents"] => {
                    let req = ClientRequest::ListAgents;
                    write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
                    write_half.write_all(b"\n").await?;
                }
                ["spawn", id] => {
                    let req = ClientRequest::Command {
                        cmd: Command::Spawn {
                            id: AgentId::new(*id),
                        },
                    };
                    write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
                    write_half.write_all(b"\n").await?;
                    let _ = printer_for_cli.print(format!("spawn requested: {id}"));
                }
                ["kill", id] => {
                    let req = ClientRequest::Command {
                        cmd: Command::Kill {
                            id: AgentId::new(*id),
                        },
                    };
                    write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
                    write_half.write_all(b"\n").await?;
                    let _ = printer_for_cli.print(format!("kill requested: {id}"));
                }
                ["purge", id] => {
                    let req = ClientRequest::Command {
                        cmd: Command::Purge {
                            id: AgentId::new(*id),
                            confirm: false,
                        },
                    };
                    write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
                    write_half.write_all(b"\n").await?;
                    let _ = printer_for_cli.print(format!(
                        "purge requested: {id}. confirm with: /purge {id} yes (within 30s)"
                    ));
                }
                ["purge", id, "yes"] => {
                    let req = ClientRequest::Command {
                        cmd: Command::Purge {
                            id: AgentId::new(*id),
                            confirm: true,
                        },
                    };
                    write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
                    write_half.write_all(b"\n").await?;
                    let _ = printer_for_cli.print(format!("purge confirm sent: {id}"));
                }
                ["join", id, topic] => {
                    let req = ClientRequest::Command {
                        cmd: Command::JoinTopic {
                            id: AgentId::new(*id),
                            topic: Topic::new(*topic),
                        },
                    };
                    write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
                    write_half.write_all(b"\n").await?;
                    let _ = printer_for_cli.print(format!("join requested: {id} -> {topic}"));
                }
                ["sleep", id] => {
                    let req = ClientRequest::Command {
                        cmd: Command::Sleep {
                            id: AgentId::new(*id),
                        },
                    };
                    write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
                    write_half.write_all(b"\n").await?;
                    let _ = printer_for_cli.print(format!("sleep requested: {id}"));
                }
                ["wake", id] => {
                    let req = ClientRequest::Command {
                        cmd: Command::Wake {
                            id: AgentId::new(*id),
                        },
                    };
                    write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
                    write_half.write_all(b"\n").await?;
                    let _ = printer_for_cli.print(format!("wake requested: {id}"));
                }
                ["topic", topic] => {
                    let t = Topic::new(*topic);
                    {
                        let mut cur = current_topic.lock().unwrap();
                        *cur = t.clone();
                    }
                    {
                        let mut pt = prompt_topic.lock().unwrap();
                        *pt = t.to_string();
                    }
                    let _ = printer_for_cli.print(format!("switched topic: {t}"));
                }
                ["dm", peer] => {
                    let dm_topic = Topic::dm(user_dm_id(), *peer);
                    {
                        let mut cur = current_topic.lock().unwrap();
                        *cur = dm_topic.clone();
                    }
                    {
                        let mut pt = prompt_topic.lock().unwrap();
                        *pt = dm_topic.to_string();
                    }

                    let join = ClientRequest::Command {
                        cmd: Command::JoinTopic {
                            id: AgentId::new(*peer),
                            topic: dm_topic,
                        },
                    };
                    write_half
                        .write_all(serde_json::to_string(&join)?.as_bytes())
                        .await?;
                    write_half.write_all(b"\n").await?;
                    let _ = printer_for_cli.print(format!("switched to dm with {peer}"));
                }
                ["spawn"] => {
                    let _ = printer_for_cli.print("usage: /spawn <AgentId>".to_string());
                }
                ["kill"] => {
                    let _ = printer_for_cli.print("usage: /kill <AgentId>".to_string());
                }
                ["purge"] => {
                    let _ = printer_for_cli
                        .print("usage: /purge <AgentId>  (then: /purge <AgentId> yes)".to_string());
                }
                ["join"] => {
                    let _ = printer_for_cli.print("usage: /join <AgentId> <topic>".to_string());
                }
                ["sleep"] => {
                    let _ = printer_for_cli.print("usage: /sleep <AgentId>".to_string());
                }
                ["wake"] => {
                    let _ = printer_for_cli.print("usage: /wake <AgentId>".to_string());
                }
                ["topic"] => {
                    let _ = printer_for_cli.print("usage: /topic <topic>".to_string());
                }
                ["dm"] => {
                    let _ = printer_for_cli.print("usage: /dm <AgentId>".to_string());
                }
                _ => {
                    let _ = printer_for_cli.print(format!("unknown command: /{rest}"));
                }
            }
            continue;
        }

        let topic = { current_topic.lock().unwrap().clone() };
        let req = ClientRequest::Publish {
            topic,
            text: line,
            sender: Sender::UserSudo,
        };
        write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
    }

    Ok(())
}
