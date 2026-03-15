use async_trait::async_trait;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use unicos_common::UnicError;

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
            let output = Command::new("bash").arg("-lc").arg(args.cmd).output().await?;
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
