use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::env;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};
use unicos_agent::{AgentConfig, AgentRuntime};
use unicos_bus::Bus;
use unicos_common::{AgentId, Command, Envelope, Event, Message, Sender, Topic, UnicError};
use unicos_llm::{LlmProvider, ResponsesProvider, RuleBasedProvider};
use unicos_tools::ToolRegistry;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathsConfig {
    pub unics_dir: PathBuf,
    pub god_log: PathBuf,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            unics_dir: PathBuf::from("/etc/unicos/unics"),
            god_log: PathBuf::from("/var/lib/unicos/god.log"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BusConfig {
    pub capacity: usize,
}

impl Default for BusConfig {
    fn default() -> Self {
        Self { capacity: 1024 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/tmp/unicos.sock"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentDefaults {
    pub perception_window: usize,
    pub default_topics: Vec<String>,
    pub tools_enabled: bool,
}

impl Default for AgentDefaults {
    fn default() -> Self {
        Self {
            perception_window: 40,
            default_topics: vec!["#general".to_string()],
            tools_enabled: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponsesLlmConfig {
    pub base_url: String,
    pub api_key_env: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub retries: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmConfig {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub responses: Option<ResponsesLlmConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub paths: PathsConfig,
    #[serde(default)]
    pub bus: BusConfig,
    #[serde(default)]
    pub agent: AgentDefaults,
    #[serde(default)]
    pub llm: Option<LlmConfig>,
    #[serde(default)]
    pub daemon: DaemonConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            paths: PathsConfig::default(),
            bus: BusConfig::default(),
            agent: AgentDefaults::default(),
            llm: None,
            daemon: DaemonConfig::default(),
        }
    }
}

impl Config {
    fn apply_env_overrides(&mut self) {
        if let Some(v) = env::var_os("UNICOS_UNICS_DIR") {
            let s = v.to_string_lossy().trim().to_string();
            if !s.is_empty() {
                self.paths.unics_dir = PathBuf::from(s);
            }
        }
        if let Some(v) = env::var_os("UNICOS_GOD_LOG") {
            let s = v.to_string_lossy().trim().to_string();
            if !s.is_empty() {
                self.paths.god_log = PathBuf::from(s);
            }
        }
    }

    pub async fn load_or_default(path: impl AsRef<Path>) -> Result<Self, UnicError> {
        let path = path.as_ref();
        let mut cfg = match tokio::fs::read_to_string(path).await {
            Ok(s) => toml::from_str(&s)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.into()),
        };
        cfg.apply_env_overrides();
        Ok(cfg)
    }
}

pub struct GodLogger {
    path: PathBuf,
}

impl GodLogger {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn spawn(self, mut rx: broadcast::Receiver<Envelope<Event>>, cancel: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Some(parent) = self.path.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    error!("god logger: create dir failed: {e}");
                    return;
                }
            }
            let mut file = match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    error!("god logger: open failed: {e}");
                    return;
                }
            };

            use tokio::io::AsyncWriteExt;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    msg = rx.recv() => {
                        let env = match msg {
                            Ok(v) => v,
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => return,
                        };
                        let line = match serde_json::to_string(&env) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        if let Err(e) = file.write_all(line.as_bytes()).await {
                            error!("god logger: write failed: {e}");
                            return;
                        }
                        if let Err(e) = file.write_all(b"\n").await {
                            error!("god logger: write newline failed: {e}");
                            return;
                        }
                    }
                }
            }
        })
    }
}

#[derive(Debug)]
struct AgentHandle {
    cancel: CancellationToken,
    abort: AbortHandle,
}

#[derive(Default)]
struct AgentRegistry {
    agents: HashMap<AgentId, AgentHandle>,
}

impl AgentRegistry {
    fn contains(&self, id: &AgentId) -> bool {
        self.agents.contains_key(id)
    }

    fn insert(&mut self, id: AgentId, handle: AgentHandle) {
        self.agents.insert(id, handle);
    }

    fn remove(&mut self, id: &AgentId) -> Option<AgentHandle> {
        self.agents.remove(id)
    }

    #[allow(dead_code)]
    fn iter_ids(&self) -> impl Iterator<Item = AgentId> + '_ {
        self.agents.keys().cloned()
    }
}

pub struct FsWatcher {
    _watcher: RecommendedWatcher,
}

impl FsWatcher {
    pub fn spawn(unics_dir: PathBuf, tx: mpsc::Sender<FsEvent>) -> Result<Self, UnicError> {
        let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(ev) = res {
                let paths = ev.paths.clone();
                let kind = ev.kind.clone();
                let _ = tx.try_send(FsEvent { kind: format!("{kind:?}"), paths });
            }
        })
        .map_err(|e| UnicError::Notify(e.to_string()))?;

        watcher
            .watch(&unics_dir, RecursiveMode::NonRecursive)
            .map_err(|e| UnicError::Notify(e.to_string()))?;

        Ok(Self { _watcher: watcher })
    }
}

#[derive(Debug)]
pub struct FsEvent {
    pub kind: String,
    pub paths: Vec<PathBuf>,
}

pub struct Orchestrator {
    cfg: Config,
    bus: Bus,
    tools: Arc<ToolRegistry>,
    provider: Arc<dyn LlmProvider>,
    registry: AgentRegistry,
    cancel: CancellationToken,
    agent_exit_tx: mpsc::Sender<AgentExit>,
    agent_exit_rx: Option<mpsc::Receiver<AgentExit>>,
    pending_purge: HashMap<AgentId, Instant>,
}

impl Orchestrator {
    pub fn new(cfg: Config) -> Self {
        let bus = Bus::new(cfg.bus.capacity);
        let tools = Arc::new(ToolRegistry::with_defaults());
        let provider: Arc<dyn LlmProvider> = match cfg.llm.as_ref() {
            Some(llm) if llm.provider == "responses" => {
                if let Some(r) = llm.responses.as_ref() {
                    let mut p = ResponsesProvider::new(
                        r.base_url.clone(),
                        r.api_key_env.clone(),
                        r.api_key.clone(),
                        llm.model.clone(),
                    );
                    if let Some(t) = r.timeout_ms {
                        p.timeout_ms = t;
                    }
                    if let Some(retries) = r.retries {
                        p.max_retries = retries;
                    }
                    Arc::new(p)
                } else {
                    Arc::new(RuleBasedProvider::default())
                }
            }
            _ => Arc::new(RuleBasedProvider::default()),
        };
        let (agent_exit_tx, agent_exit_rx) = mpsc::channel(256);
        Self {
            cfg,
            bus,
            tools,
            provider,
            registry: AgentRegistry::default(),
            cancel: CancellationToken::new(),
            agent_exit_tx,
            agent_exit_rx: Some(agent_exit_rx),
            pending_purge: HashMap::new(),
        }
    }

    pub fn bus(&self) -> Bus {
        self.bus.clone()
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub async fn bootstrap_from_fs(&mut self) -> Result<(), UnicError> {
        tokio::fs::create_dir_all(&self.cfg.paths.unics_dir).await?;
        let mut rd = tokio::fs::read_dir(&self.cfg.paths.unics_dir).await?;
        while let Some(ent) = rd.next_entry().await? {
            let meta = ent.metadata().await?;
            if meta.is_dir() {
                if let Some(name) = ent.file_name().to_str() {
                    let id = AgentId::new(name.to_string());
                    if !self.registry.contains(&id) {
                        self.spawn_agent(id).await?;
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn spawn_agent(&mut self, id: AgentId) -> Result<(), UnicError> {
        if self.registry.contains(&id) {
            return Ok(());
        }
        let soul_dir = self.cfg.paths.unics_dir.join(id.as_str());
        tokio::fs::create_dir_all(&soul_dir).await?;
        self.ensure_default_agent_files(&id, &soul_dir).await?;

        let topics = self
            .cfg
            .agent
            .default_topics
            .iter()
            .map(|t| Topic::new(t))
            .collect::<Vec<_>>();

        let cfg = AgentConfig {
            id: id.clone(),
            topics,
            perception_window: self.cfg.agent.perception_window,
            soul_dir,
            tools_enabled: self.cfg.agent.tools_enabled,
        };

        let cancel = self.cancel.child_token();
        let agent = AgentRuntime::new(
            cfg,
            self.bus.clone(),
            self.provider.clone(),
            self.tools.clone(),
            cancel.clone(),
        );
        let id_clone = id.clone();
        let join = tokio::spawn(async move {
            let res = agent.run().await;
            if let Err(e) = res {
                warn!("agent {id_clone} stopped with error: {e}");
            }
        });

        let abort = join.abort_handle();
        let exit_tx = self.agent_exit_tx.clone();
        let exit_id = id.clone();
        tokio::spawn(async move {
            let kind = match join.await {
                Ok(()) => AgentExitKind::Finished,
                Err(e) if e.is_panic() => {
                    AgentExitKind::Panicked(format!("{e}"))
                }
                Err(_) => AgentExitKind::Cancelled,
            };
            let _ = exit_tx.send(AgentExit { id: exit_id, kind }).await;
        });

        self.registry.insert(
            id.clone(),
            AgentHandle {
                cancel,
                abort,
            },
        );

        self.bus.publish(self.bus.envelope(
            Topic::new("system"),
            Sender::System,
            Event::SystemNotification(unicos_common::SystemNotification::AgentSpawned { id }),
        ))?;
        Ok(())
    }

    pub async fn kill_agent(&mut self, id: AgentId) -> Result<(), UnicError> {
        if let Some(handle) = self.registry.remove(&id) {
            handle.cancel.cancel();
            handle.abort.abort();
            self.bus.publish(self.bus.envelope(
                Topic::new("system"),
                Sender::System,
                Event::SystemNotification(unicos_common::SystemNotification::AgentKilled { id }),
            ))?;
        }
        Ok(())
    }

    async fn purge_agent(&mut self, id: AgentId, confirm: bool) -> Result<(), UnicError> {
        const TTL: Duration = Duration::from_secs(30);

        if !confirm {
            self.pending_purge.insert(id, Instant::now());
            return Ok(());
        }

        let Some(t0) = self.pending_purge.remove(&id) else {
            return Ok(());
        };
        if t0.elapsed() > TTL {
            return Ok(());
        }

        self.kill_agent(id.clone()).await?;
        let soul_dir = self.cfg.paths.unics_dir.join(id.as_str());
        match tokio::fs::remove_dir_all(&soul_dir).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        self.bus.publish(self.bus.envelope(
            Topic::new("system"),
            Sender::System,
            Event::SystemNotification(unicos_common::SystemNotification::AgentPurged { id }),
        ))?;
        Ok(())
    }

    async fn ensure_default_agent_files(&self, id: &AgentId, soul_dir: &Path) -> Result<(), UnicError> {
        let soul_md = soul_dir.join("soul.md");
        match tokio::fs::metadata(&soul_md).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let content = format!(
                    r#"# {id}

You are {id}, an agent running inside UnicOS.

Rules:
- Keep replies concise.
- When the user says "Ping" (case-insensitive), reply exactly with "Pong" and nothing else.
"#
                );
                tokio::fs::write(&soul_md, content).await?;
            }
            Err(e) => return Err(e.into()),
        }

        #[derive(Serialize)]
        struct AgentDiskConfig<'a> {
            id: &'a str,
            topics: Vec<String>,
            perception_window: usize,
            tools_enabled: bool,
        }

        let cfg_json = soul_dir.join("config.json");
        match tokio::fs::metadata(&cfg_json).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = AgentDiskConfig {
                    id: id.as_str(),
                    topics: self.cfg.agent.default_topics.clone(),
                    perception_window: self.cfg.agent.perception_window,
                    tools_enabled: self.cfg.agent.tools_enabled,
                };
                let json = serde_json::to_string_pretty(&cfg)?;
                tokio::fs::write(&cfg_json, json).await?;
            }
            Err(e) => return Err(e.into()),
        }

        Ok(())
    }

    async fn handle_command(&mut self, cmd: Command) -> Result<(), UnicError> {
        match cmd {
            Command::Spawn { id } => self.spawn_agent(id).await?,
            Command::Kill { id } => self.kill_agent(id).await?,
            Command::Purge { id, confirm } => self.purge_agent(id, confirm).await?,
            Command::JoinTopic { ref id, ref topic } => {
                if topic.is_dm() {
                    let Some((a, b)) = topic.dm_participants() else {
                        return Ok(());
                    };
                    if id.as_str() != a && id.as_str() != b {
                        return Ok(());
                    }
                }
                self.bus.publish(self.bus.envelope(
                    Topic::new("system"),
                    Sender::System,
                    Event::Command(cmd),
                ))?;
            }
            Command::Sleep { .. } | Command::Wake { .. } => {
                self.bus.publish(self.bus.envelope(
                    Topic::new("system"),
                    Sender::System,
                    Event::Command(cmd),
                ))?;
            }
            Command::SetUserTopic { .. } => {}
        }
        Ok(())
    }

    pub fn spawn_god_logger(&self) -> JoinHandle<()> {
        GodLogger::new(self.cfg.paths.god_log.clone()).spawn(self.bus.subscribe(), self.cancel.clone())
    }

    pub async fn run(self) -> Result<(), UnicError> {
        self.run_with_fs_watch(true).await
    }

    pub async fn run_with_fs_watch(mut self, watch_fs: bool) -> Result<(), UnicError> {
        self.bootstrap_from_fs().await?;

        let mut agent_exit_rx = self
            .agent_exit_rx
            .take()
            .expect("agent_exit_rx already taken");
        let (fs_tx, mut fs_rx) = mpsc::channel::<FsEvent>(256);
        let _watcher = if watch_fs {
            match FsWatcher::spawn(self.cfg.paths.unics_dir.clone(), fs_tx) {
                Ok(w) => Some(w),
                Err(e) => {
                    warn!("fs watcher disabled: {e}");
                    None
                }
            }
        } else {
            None
        };

        let mut rx = self.bus.subscribe();
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return Ok(()),
                Some(exit) = agent_exit_rx.recv() => {
                    if self.cancel.is_cancelled() {
                        continue;
                    }
                    if self.registry.remove(&exit.id).is_some() {
                        match exit.kind {
                            AgentExitKind::Finished | AgentExitKind::Cancelled => {
                                self.bus.publish(self.bus.envelope(
                                    Topic::new("system"),
                                    Sender::System,
                                    Event::SystemNotification(unicos_common::SystemNotification::AgentKilled { id: exit.id }),
                                ))?;
                            }
                            AgentExitKind::Panicked(message) => {
                                self.bus.publish(self.bus.envelope(
                                    Topic::new("system"),
                                    Sender::System,
                                    Event::SystemNotification(unicos_common::SystemNotification::AgentPanicked { id: exit.id, message }),
                                ))?;
                            }
                        }
                    }
                }
                Some(fsev) = fs_rx.recv(), if watch_fs => {
                    for p in fsev.paths {
                        if p.parent() == Some(&self.cfg.paths.unics_dir) {
                            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                                let id = AgentId::new(name.to_string());
                                if fsev.kind.contains("Create") {
                                    if let Err(e) = self.spawn_agent(id).await {
                                        warn!("spawn from fs failed: {e}");
                                    }
                                } else if fsev.kind.contains("Remove") {
                                    if let Err(e) = self.kill_agent(id).await {
                                        warn!("kill from fs failed: {e}");
                                    }
                                }
                            }
                        }
                    }
                }
                recv = rx.recv() => {
                    let env = match recv {
                        Ok(v) => v,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    };

                    let Envelope { payload, sender, .. } = env;
                    match payload {
                        Event::Command(cmd) => {
                            if !matches!(sender, Sender::UserSudo) {
                                continue;
                            }
                            if let Err(e) = self.handle_command(cmd).await {
                                error!("handle command failed: {e}");
                            }
                        }
                        Event::Message(Message{..}) | Event::SystemNotification(_) => {}
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
struct AgentExit {
    id: AgentId,
    kind: AgentExitKind,
}

#[derive(Debug)]
enum AgentExitKind {
    Finished,
    Cancelled,
    Panicked(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_default_when_missing() {
        unsafe {
            std::env::remove_var("UNICOS_UNICS_DIR");
            std::env::remove_var("UNICOS_GOD_LOG");
        }
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.toml");
        let cfg = Config::load_or_default(&missing).await.unwrap();
        assert_eq!(cfg.bus.capacity, 1024);
    }

    #[tokio::test]
    async fn load_config_from_file() {
        unsafe {
            std::env::remove_var("UNICOS_UNICS_DIR");
            std::env::remove_var("UNICOS_GOD_LOG");
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let content = r##"
[bus]
capacity = 7

[agent]
perception_window = 3
default_topics = ["#general", "#project_x"]
tools_enabled = false
"##;
        tokio::fs::write(&p, content).await.unwrap();
        let cfg = Config::load_or_default(&p).await.unwrap();
        assert_eq!(cfg.bus.capacity, 7);
        assert_eq!(cfg.agent.perception_window, 3);
        assert_eq!(cfg.agent.tools_enabled, false);
        assert_eq!(cfg.agent.default_topics.len(), 2);
    }

    #[tokio::test]
    async fn env_overrides_paths() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        tokio::fs::write(&p, " ").await.unwrap();

        unsafe {
            std::env::set_var("UNICOS_UNICS_DIR", "/tmp/unicos-env-unics");
            std::env::set_var("UNICOS_GOD_LOG", "/tmp/unicos-env-god.log");
        }
        let cfg = Config::load_or_default(&p).await.unwrap();
        assert_eq!(cfg.paths.unics_dir, PathBuf::from("/tmp/unicos-env-unics"));
        assert_eq!(cfg.paths.god_log, PathBuf::from("/tmp/unicos-env-god.log"));
        unsafe {
            std::env::remove_var("UNICOS_UNICS_DIR");
            std::env::remove_var("UNICOS_GOD_LOG");
        }
    }

    #[tokio::test]
    async fn spawn_creates_default_soul_files() {
        let tmp = tempfile::tempdir().unwrap();
        let unics_dir = tmp.path().join("unics");
        let god_log = tmp.path().join("god.log");
        tokio::fs::create_dir_all(&unics_dir).await.unwrap();

        let mut cfg = Config::default();
        cfg.paths.unics_dir = unics_dir.clone();
        cfg.paths.god_log = god_log;
        cfg.llm = None;

        let mut orch = Orchestrator::new(cfg);
        orch.spawn_agent(AgentId::new("Alice")).await.unwrap();

        let soul_dir = unics_dir.join("Alice");
        assert!(tokio::fs::try_exists(soul_dir.join("soul.md"))
            .await
            .unwrap());
        assert!(tokio::fs::try_exists(soul_dir.join("config.json"))
            .await
            .unwrap());

        orch.kill_agent(AgentId::new("Alice")).await.unwrap();
        orch.cancel.cancel();
    }

    #[tokio::test]
    async fn purge_requires_two_step_confirm() {
        let tmp = tempfile::tempdir().unwrap();
        let unics_dir = tmp.path().join("unics");
        tokio::fs::create_dir_all(&unics_dir).await.unwrap();

        let mut cfg = Config::default();
        cfg.paths.unics_dir = unics_dir.clone();
        cfg.paths.god_log = tmp.path().join("god.log");
        cfg.llm = None;

        let mut orch = Orchestrator::new(cfg);
        orch.spawn_agent(AgentId::new("Bob")).await.unwrap();
        let soul_dir = unics_dir.join("Bob");
        assert!(tokio::fs::try_exists(&soul_dir).await.unwrap());

        orch.handle_command(Command::Purge {
            id: AgentId::new("Bob"),
            confirm: true,
        })
        .await
        .unwrap();
        assert!(tokio::fs::try_exists(&soul_dir).await.unwrap());

        orch.handle_command(Command::Purge {
            id: AgentId::new("Bob"),
            confirm: false,
        })
        .await
        .unwrap();
        orch.handle_command(Command::Purge {
            id: AgentId::new("Bob"),
            confirm: true,
        })
        .await
        .unwrap();
        assert!(!tokio::fs::try_exists(&soul_dir).await.unwrap());

        orch.cancel.cancel();
    }
}
