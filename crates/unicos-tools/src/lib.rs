use async_trait::async_trait;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use unicos_common::{ConversationId, Envelope, Event, UnicError};
use uuid::Uuid;

#[async_trait]
pub trait SystemTool: Send + Sync {
    fn name(&self) -> &'static str;
    async fn call(&self, args: Value) -> Result<Value, UnicError>;
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn SystemTool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        let mut reg = Self::new();
        reg.register(BashExecutor::default());
        reg.register(FileSystemOperator::default());
        reg.register(NetRequestTool::default());
        reg.register(ConversationLoader::default());
        reg
    }

    pub fn register<T: SystemTool + 'static>(&mut self, tool: T) {
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn SystemTool>> {
        self.tools.get(name).cloned()
    }

    pub async fn call(&self, name: &str, args: Value) -> Result<Value, UnicError> {
        let tool = self
            .get(name)
            .ok_or_else(|| UnicError::Tool(format!("unknown tool: {name}")))?;
        tool.call(args).await
    }
}

#[derive(Debug, Default)]
pub struct BashExecutor;

#[derive(Debug, Deserialize)]
struct BashArgs {
    cmd: String,
    timeout_ms: Option<u64>,
    cwd: Option<PathBuf>,
}

#[async_trait]
impl SystemTool for BashExecutor {
    fn name(&self) -> &'static str {
        "bash"
    }

    async fn call(&self, args: Value) -> Result<Value, UnicError> {
        let args: BashArgs = serde_json::from_value(args)?;
        let timeout_ms = args.timeout_ms.unwrap_or(30_000);

        let fut = async move {
            let mut cmd = Command::new("bash");
            cmd.arg("-lc").arg(args.cmd);
            if let Some(cwd) = args.cwd {
                cmd.current_dir(cwd);
            }
            let output = cmd.output().await?;
            Ok::<_, std::io::Error>(output)
        };

        let output = timeout(Duration::from_millis(timeout_ms), fut)
            .await
            .map_err(|_| UnicError::Tool("bash: timeout".to_string()))??;

        Ok(json!({
            "status": if output.status.success() { "ok" } else { "error" },
            "exit_code": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }))
    }
}

#[derive(Debug, Default)]
pub struct FileSystemOperator;

#[derive(Debug, Deserialize)]
#[serde(tag = "op")]
enum FsArgs {
    #[serde(rename = "read")]
    Read { path: PathBuf, max_bytes: Option<usize> },
    #[serde(rename = "write")]
    Write {
        path: PathBuf,
        data: String,
        append: Option<bool>,
    },
}

#[async_trait]
impl SystemTool for FileSystemOperator {
    fn name(&self) -> &'static str {
        "fs"
    }

    async fn call(&self, args: Value) -> Result<Value, UnicError> {
        let args: FsArgs = serde_json::from_value(args)?;
        match args {
            FsArgs::Read { path, max_bytes } => {
                let bytes = tokio::fs::read(&path).await?;
                let truncated = max_bytes.is_some_and(|max| bytes.len() > max);
                let bytes = match max_bytes {
                    Some(max) if bytes.len() > max => &bytes[..max],
                    _ => &bytes,
                };
                Ok(json!({
                    "path": path,
                    "data": String::from_utf8_lossy(bytes),
                    "truncated": truncated,
                }))
            }
            FsArgs::Write { path, data, append } => {
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                if append.unwrap_or(false) {
                    use tokio::io::AsyncWriteExt;
                    let mut f = tokio::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .await?;
                    f.write_all(data.as_bytes()).await?;
                    Ok(json!({ "path": path, "status": "appended" }))
                } else {
                    tokio::fs::write(&path, data).await?;
                    Ok(json!({ "path": path, "status": "written" }))
                }
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct NetRequestTool {
    client: reqwest::Client,
}

#[derive(Debug, Deserialize, Serialize)]
struct NetArgs {
    method: String,
    url: String,
    headers: Option<HashMap<String, String>>,
    body: Option<Value>,
}

#[async_trait]
impl SystemTool for NetRequestTool {
    fn name(&self) -> &'static str {
        "net"
    }

    async fn call(&self, args: Value) -> Result<Value, UnicError> {
        let args: NetArgs = serde_json::from_value(args)?;
        let method: Method = args
            .method
            .parse()
            .map_err(|_| UnicError::Tool(format!("net: invalid method {}", args.method)))?;

        let mut req = self.client.request(method, args.url);
        if let Some(headers) = args.headers {
            for (k, v) in headers {
                req = req.header(k, v);
            }
        }
        if let Some(body) = args.body {
            req = req.json(&body);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| UnicError::Tool(format!("net: request failed: {e}")))?;

        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect::<HashMap<_, _>>();
        let body = resp
            .text()
            .await
            .map_err(|e| UnicError::Tool(format!("net: read body failed: {e}")))?;

        Ok(json!({
            "status": status,
            "headers": headers,
            "body": body,
        }))
    }
}

#[derive(Debug, Default)]
pub struct ConversationLoader;

#[derive(Debug, Deserialize)]
struct ConvLoadArgs {
    conversation_id: String,
    max_messages: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ConversationFile {
    conversation_id: ConversationId,
    message_ids: Vec<Uuid>,
}

#[async_trait]
impl SystemTool for ConversationLoader {
    fn name(&self) -> &'static str {
        "conv_load"
    }

    async fn call(&self, args: Value) -> Result<Value, UnicError> {
        let args: ConvLoadArgs = serde_json::from_value(args)?;
        let conv_id = ConversationId::parse_hex(&args.conversation_id)
            .unwrap_or_else(|| ConversationId::from_seed(&args.conversation_id));

        let conversations_dir = std::env::var_os("UNICOS_CONVERSATIONS_DIR")
            .map(|v| v.to_string_lossy().trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/var/lib/unicos/conversations".to_string());
        let conversations_dir = PathBuf::from(conversations_dir);

        let god_log = std::env::var_os("UNICOS_GOD_LOG")
            .map(|v| v.to_string_lossy().trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/var/lib/unicos/god.log".to_string());
        let god_log = PathBuf::from(god_log);

        let conv_path = conversations_dir.join(format!("{}.json", conv_id.as_str()));
        let conv_json = tokio::fs::read_to_string(&conv_path).await?;
        let conv_file: ConversationFile = serde_json::from_str(&conv_json)?;
        if conv_file.conversation_id != conv_id {
            return Err(UnicError::Tool(format!(
                "conv_load: conversation_id mismatch in {}",
                conv_path.display()
            )));
        }

        let mut wanted = conv_file
            .message_ids
            .into_iter()
            .collect::<Vec<Uuid>>();
        let max = args.max_messages.unwrap_or(200).max(1);
        if wanted.len() > max {
            wanted = wanted[wanted.len() - max..].to_vec();
        }
        let wanted_set = wanted.iter().cloned().collect::<HashSet<Uuid>>();

        let god = tokio::fs::read_to_string(&god_log).await?;
        let mut by_id: HashMap<Uuid, Value> = HashMap::new();
        for line in god.lines() {
            let Ok(env) = serde_json::from_str::<Envelope<Event>>(line) else { continue };
            if !wanted_set.contains(&env.id) {
                continue;
            }
            let Event::Message(m) = env.payload else { continue };
            by_id.insert(
                env.id,
                json!({
                    "id": env.id,
                    "ts": env.ts,
                    "topic": env.topic,
                    "sender": env.sender,
                    "conversation_id": env.conversation_id,
                    "text": m.text,
                }),
            );
        }

        let mut messages = Vec::new();
        let mut missing = Vec::new();
        for id in wanted {
            if let Some(v) = by_id.get(&id) {
                messages.push(v.clone());
            } else {
                missing.push(id);
            }
        }

        Ok(json!({
            "conversation_id": conv_id,
            "conv_path": conv_path,
            "god_log": god_log,
            "messages": messages,
            "missing_message_ids": missing,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use unicos_common::{Message, Sender, Topic};

    #[tokio::test]
    async fn conv_load_reads_conversation_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let conv_dir = tmp.path().join("conversations");
        let god_log = tmp.path().join("god.log");
        tokio::fs::create_dir_all(&conv_dir).await.unwrap();

        unsafe {
            std::env::set_var("UNICOS_CONVERSATIONS_DIR", conv_dir.to_string_lossy().to_string());
            std::env::set_var("UNICOS_GOD_LOG", god_log.to_string_lossy().to_string());
        }

        let conv_id = ConversationId::from_seed("task-42");
        let env = Envelope {
            id: Uuid::now_v7(),
            ts: chrono::Utc::now(),
            topic: Topic::new("general"),
            sender: Sender::UserSudo,
            conversation_id: conv_id.clone(),
            payload: Event::Message(Message {
                text: "hello".to_string(),
            }),
        };

        let god_line = serde_json::to_string(&env).unwrap();
        tokio::fs::write(&god_log, format!("{god_line}\n")).await.unwrap();

        let conv_path = conv_dir.join(format!("{}.json", conv_id.as_str()));
        tokio::fs::write(
            &conv_path,
            serde_json::to_string_pretty(&json!({
                "conversation_id": conv_id.clone(),
                "message_ids": [env.id],
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        let tool = ConversationLoader::default();
        let out = tool
            .call(json!({"conversation_id": conv_id.to_string(), "max_messages": 10}))
            .await
            .unwrap();
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert_eq!(out["missing_message_ids"].as_array().unwrap().len(), 0);
        assert_eq!(out["messages"][0]["text"].as_str().unwrap(), "hello");

        unsafe {
            std::env::remove_var("UNICOS_CONVERSATIONS_DIR");
            std::env::remove_var("UNICOS_GOD_LOG");
        }
    }
}
