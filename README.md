# UnicOS (RFC0000 / RFC0001 MVP)

该仓库实现了 RFC0000 与 RFC0001 中描述的最小可运行骨架：Tokio 广播总线 + 核心调度器 + 多 Agent 运行时 + 工具箱 + 上帝日志。

## 运行（守护进程 + CLI）

守护进程：

```bash
cargo run -p unicosd -- --config /etc/unicos/config.toml
```

CLI（另开一个终端）：

```bash
cargo run -p unicos-cli -- --socket /tmp/unicos.sock
```

守护进程默认读取 `/etc/unicos/config.toml`（不存在则使用内置默认值）。

`paths` 通过环境变量配置：
- `UNICOS_UNICS_DIR`：Agent 灵魂区目录（默认 `/etc/unicos/unics`）
- `UNICOS_GOD_LOG`：上帝日志路径（默认 `/var/lib/unicos/god.log`）

要接入 `/responses` 模型提供商，参考 `config.example.toml` 增加 `[llm]` 与 `[llm.responses]` 配置。

## 交互

- 普通输入：向当前 topic 发言（默认 `#general`）
- 管理命令（以 `/` 开头）：
  - `/topic <name>`
  - `/spawn <AgentId>`
  - `/kill <AgentId>`
  - `/join <AgentId> <topic>`
  - `/sleep <AgentId>`
  - `/wake <AgentId>`

## Agent 灵魂区

内核会以 `$UNICOS_UNICS_DIR/<AgentId>/` 作为 Agent 的灵魂目录，Agent 会读取其中的 `soul.md` 注入系统提示词。

## Ping/Pong（只改 SOUL.md）

1. 准备一个配置文件（把 `base_url` / `api_key_env` 改成你在 `~/chobits/config.toml` 里的值，模型用 `gpt-5.2`）：

```toml
[daemon]
socket_path = "/tmp/unicos.sock"

[bus]
capacity = 1024

[agent]
perception_window = 40
default_topics = ["#general"]
tools_enabled = true

[llm]
provider = "responses"
model = "gpt-5.2"

[llm.responses]
base_url = "https://co.yes.vg/v1"
api_key_env = "APIROUTER_API_KEY"
retries = 8
```

2. 运行守护进程：

```bash
export UNICOS_UNICS_DIR=/tmp/unicos-unics
export UNICOS_GOD_LOG=/tmp/unicos-god.log
cargo run -p unicosd -- --config /tmp/unicos-config.toml
```

3. 另开一个终端运行 CLI（同一个 socket）：

```bash
cargo run -p unicos-cli -- --socket /tmp/unicos.sock
```

4. 在 CLI 里输入：

```text
/spawn Pinger
```

5. 只通过写 `SOUL.md` 配置它的行为：

```bash
mkdir -p /tmp/unicos-unics/Pinger
cat > /tmp/unicos-unics/Pinger/soul.md <<'EOF'
You are Pinger.
When the user says "Ping" (case-insensitive), reply exactly with "Pong" and nothing else.
EOF
```

6. 回到 CLI 输入 `Ping`，应看到该 Agent 输出 `Pong`。

## 工具调用（MVP 约定）

当 Agent 判定需要响应时，如最近一条用户消息满足以下前缀，将触发工具调用：

- `!bash <cmd>`
- `!fs_read <path>`
- `!net_get <url>`

工具返回会以普通消息形式回流到总线。
