use reedline::{
    default_emacs_keybindings, ColumnarMenu, Completer, EditCommand, Emacs, ExternalPrinter,
    Highlighter, KeyCode, KeyModifiers, Keybindings, Prompt, PromptEditMode, PromptHistorySearch,
    MenuBuilder, Reedline, ReedlineEvent, ReedlineMenu, Signal, Span, StyledText, Suggestion,
};
use std::borrow::Cow;
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
    topics: Vec<&'static str>,
}

impl Default for UnicosCompleter {
    fn default() -> Self {
        Self {
            commands: vec![
                ("/ping", "daemon connectivity"),
                ("/topic", "switch current topic"),
                ("/spawn", "spawn agent"),
                ("/kill", "kill agent"),
                ("/join", "agent join topic"),
                ("/sleep", "sleep agent"),
                ("/wake", "wake agent"),
                ("/quit", "exit cli"),
                ("/exit", "exit cli"),
            ],
            topics: vec!["#general", "#system", "#project_x"],
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
            let completing_topic =
                (matches!(cmd, "/topic") && arg_index == 1) || (matches!(cmd, "/join") && arg_index == 2);
            if completing_topic {
                for t in &self.topics {
                    if t.starts_with(prefix) || prefix.is_empty() {
                        out.push(Suggestion {
                            value: (*t).to_string(),
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

    let printer = ExternalPrinter::default();
    let printer_for_server = printer.clone();

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
                    if let Some(s) = format_event(envelope) {
                        let _ = printer_for_server.print(s);
                    }
                }
                ServerMessage::Pong => {
                    let _ = printer_for_server.print("Pong".to_string());
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
            .with_completer(Box::new(UnicosCompleter::default()))
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
        if let Some(rest) = line.strip_prefix('/') {
            let parts = rest.split_whitespace().collect::<Vec<_>>();
            match parts.as_slice() {
                ["ping"] => {
                    let req = ClientRequest::Ping;
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
                }
                ["kill", id] => {
                    let req = ClientRequest::Command {
                        cmd: Command::Kill {
                            id: AgentId::new(*id),
                        },
                    };
                    write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
                    write_half.write_all(b"\n").await?;
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
                }
                ["sleep", id] => {
                    let req = ClientRequest::Command {
                        cmd: Command::Sleep {
                            id: AgentId::new(*id),
                        },
                    };
                    write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
                    write_half.write_all(b"\n").await?;
                }
                ["wake", id] => {
                    let req = ClientRequest::Command {
                        cmd: Command::Wake {
                            id: AgentId::new(*id),
                        },
                    };
                    write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
                    write_half.write_all(b"\n").await?;
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
                }
                _ => {}
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
