use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{info, warn};
use unicos_common::ipc::{ClientRequest, ServerMessage};
use unicos_common::{Event, Message, Sender, Topic, UnicError};
use unicos_core::Config;
use std::collections::BTreeSet;
use std::sync::Arc;

fn parse_args() -> (std::path::PathBuf, Option<std::path::PathBuf>, bool) {
    let mut config = std::path::PathBuf::from("/etc/unicos/config.toml");
    let mut socket: Option<std::path::PathBuf> = None;
    let mut no_fs_watch = false;
    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => {
                if let Some(p) = args.next() {
                    config = std::path::PathBuf::from(p);
                }
            }
            "--socket" => {
                if let Some(p) = args.next() {
                    socket = Some(std::path::PathBuf::from(p));
                }
            }
            "--no-fs-watch" => no_fs_watch = true,
            _ => {}
        }
    }
    (config, socket, no_fs_watch)
}

async fn handle_client(
    stream: UnixStream,
    bus: unicos_bus::Bus,
    agent_index: Arc<Mutex<BTreeSet<String>>>,
) -> Result<(), UnicError> {
    let (read_half, write_half) = stream.into_split();
    let write_half = std::sync::Arc::new(Mutex::new(write_half));

    // Forward bus events to client.
    let mut rx = bus.subscribe();
    let write_half_events = std::sync::Arc::clone(&write_half);
    tokio::spawn(async move {
        loop {
            let env = match rx.recv().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let msg = ServerMessage::Event { envelope: env };
            let Ok(line) = serde_json::to_string(&msg) else { continue };
            let mut w = write_half_events.lock().await;
            if w.write_all(line.as_bytes()).await.is_err() {
                return;
            }
            if w.write_all(b"\n").await.is_err() {
                return;
            }
        }
    });

    // Read client requests.
    let mut lines = BufReader::new(read_half).lines();
    while let Some(line) = lines.next_line().await? {
        let req = match serde_json::from_str::<ClientRequest>(&line) {
            Ok(r) => r,
            Err(e) => {
                let msg = ServerMessage::Error {
                    message: format!("bad request: {e}"),
                };
                let mut w = write_half.lock().await;
                let _ = w
                    .write_all(serde_json::to_string(&msg).unwrap().as_bytes())
                    .await;
                let _ = w.write_all(b"\n").await;
                continue;
            }
        };

        match req {
            ClientRequest::Ping => {
                let msg = ServerMessage::Pong;
                let mut w = write_half.lock().await;
                w.write_all(serde_json::to_string(&msg).unwrap().as_bytes())
                    .await?;
                w.write_all(b"\n").await?;
            }
            ClientRequest::ListAgents => {
                let ids = {
                    let idx = agent_index.lock().await;
                    idx.iter().cloned().map(unicos_common::AgentId::new).collect::<Vec<_>>()
                };
                let msg = ServerMessage::Agents { ids };
                let mut w = write_half.lock().await;
                w.write_all(serde_json::to_string(&msg).unwrap().as_bytes())
                    .await?;
                w.write_all(b"\n").await?;
            }
            ClientRequest::Publish { topic, text, sender, conversation_id } => {
                bus.publish(bus.envelope_with_conversation(
                    topic,
                    sender,
                    conversation_id,
                    Event::Message(Message { text }),
                ))?;
            }
            ClientRequest::Command { cmd } => {
                bus.publish(bus.envelope(
                    Topic::new("system"),
                    Sender::UserSudo,
                    Event::Command(cmd),
                ))?;
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let (config_path, socket_override, no_fs_watch) = parse_args();
    let cfg = Config::load_or_default(&config_path).await?;
    let socket_path = socket_override.unwrap_or_else(|| cfg.daemon.socket_path.clone());

    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if tokio::fs::try_exists(&socket_path).await.unwrap_or(false) {
        let _ = tokio::fs::remove_file(&socket_path).await;
    }

    let orchestrator = unicos_core::Orchestrator::new(cfg);
    let bus = orchestrator.bus();
    let cancel = orchestrator.cancel_token();
    let _god = orchestrator.spawn_god_logger();
    let _convs = orchestrator.spawn_conversation_aggregator();

    let mut core_task = tokio::spawn(async move {
        if no_fs_watch {
            warn!("--no-fs-watch: still bootstraps once from unics_dir");
        }
        let res = orchestrator.run_with_fs_watch(!no_fs_watch).await;
        if let Err(e) = res {
            warn!("orchestrator stopped: {e}");
        }
    });

    let listener = UnixListener::bind(&socket_path)?;
    info!("unicosd listening on {}", socket_path.display());

    let server_bus = bus.clone();
    let agent_index: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));

    // Maintain an index of running agents for list/completion.
    {
        let agent_index = Arc::clone(&agent_index);
        let mut rx = bus.subscribe();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    recv = rx.recv() => {
                        let Ok(env) = recv else { continue };
                        if let Event::SystemNotification(n) = env.payload {
                            match n {
                                unicos_common::SystemNotification::AgentSpawned { id } => {
                                    agent_index.lock().await.insert(id.to_string());
                                }
                                unicos_common::SystemNotification::AgentKilled { id } => {
                                    agent_index.lock().await.remove(id.as_str());
                                }
                                unicos_common::SystemNotification::AgentPurged { id } => {
                                    agent_index.lock().await.remove(id.as_str());
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        });
    }

    let mut server_task = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let bus = server_bus.clone();
            let agent_index = Arc::clone(&agent_index);
            tokio::spawn(async move {
                if let Err(e) = handle_client(stream, bus, agent_index).await {
                    warn!("client handler error: {e}");
                }
            });
        }
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received, shutting down");
        }
        _ = &mut server_task => {
            warn!("server stopped");
        }
        _ = &mut core_task => {
            warn!("core stopped");
        }
    }

    cancel.cancel();
    let _ = tokio::fs::remove_file(&socket_path).await;
    Ok(())
}
