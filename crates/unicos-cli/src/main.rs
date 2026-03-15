use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::warn;
use unicos_common::ipc::{ClientRequest, ServerMessage};
use unicos_common::{AgentId, Command, Event, Sender, Topic};

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

fn print_event(env: unicos_common::Envelope<Event>) {
    match env.payload {
        Event::Message(msg) => {
            println!("{} {} {} {}", env.ts, env.topic, env.sender.label(), msg.text);
        }
        Event::SystemNotification(n) => {
            println!("{} {} [SystemNotice] {:?}", env.ts, env.topic, n);
        }
        Event::Command(_) => {}
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let socket = parse_args();
    let stream = UnixStream::connect(&socket).await?;
    let (read_half, mut write_half) = stream.into_split();

    // server -> stdout
    tokio::spawn(async move {
        let mut lines = BufReader::new(read_half).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg = match serde_json::from_str::<ServerMessage>(&line) {
                Ok(m) => m,
                Err(e) => {
                    warn!("bad server message: {e}: {line}");
                    continue;
                }
            };
            match msg {
                ServerMessage::Event { envelope } => print_event(envelope),
                ServerMessage::Pong => println!("Pong"),
                ServerMessage::Error { message } => eprintln!("[daemon error] {message}"),
            }
        }
    });

    eprintln!("Connected to {socket}. Use /spawn /kill /join /sleep /wake /topic; plain text sends to current topic.");

    let mut current_topic = Topic::new("general");
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
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
                    current_topic = Topic::new(*topic);
                    println!("[User] switched to topic {current_topic}");
                }
                _ => println!("unknown command: /{rest}"),
            }
            continue;
        }

        let req = ClientRequest::Publish {
            topic: current_topic.clone(),
            text: line,
            sender: Sender::UserSudo,
        };
        write_half.write_all(serde_json::to_string(&req)?.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
    }

    Ok(())
}

