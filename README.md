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

`unicos-cli` 使用 `reedline` 提供输入高亮与 Tab 补全（按 `Tab` 打开补全菜单，`/quit` 退出）。

守护进程默认读取 `/etc/unicos/config.toml`（不存在则使用内置默认值）。

`paths` 通过环境变量配置：
- `UNICOS_UNICS_DIR`：Agent 灵魂区目录（默认 `/etc/unicos/unics`）
- `UNICOS_GOD_LOG`：上帝日志路径（默认 `/var/lib/unicos/god.log`）
- `UNICOS_CONVERSATIONS_DIR`：会话聚合目录（默认 `/var/lib/unicos/conversations`）

要接入 `/responses` 模型提供商，参考 `config.example.toml` 增加 `[llm]` 与 `[llm.responses]` 配置。

## 交互

- 普通输入：向当前 topic 发言（默认 `#general`）
- 管理命令（以 `/` 开头）：
  - `/agents`（列出当前运行中的 Agent）
  - `/dm <AgentId>`（切到与该 Agent 的私聊 topic，并让它 join）
  - `/topic <name>`
  - `/conv <seed-or-8hex-id>`（切换当前 conversation_id；会在后台聚合成 `{conv_id}.json`）
  - `/spawn <AgentId>`
  - `/kill <AgentId>`
  - `/purge <AgentId>`（两步确认：先请求，再 `/purge <AgentId> yes` 真正删除磁盘目录）
  - `/join <AgentId> <topic>`
  - `/sleep <AgentId>`
  - `/wake <AgentId>`
  - `@<AgentId> <text>`（不切 topic，直接发私聊）

## Agent 灵魂区

内核会以 `$UNICOS_UNICS_DIR/<AgentId>/` 作为 Agent 的灵魂目录，Agent 会读取其中的 `soul.md` 注入系统提示词。

`/spawn <AgentId>` 会在该目录下自动生成默认的 `soul.md` 与 `config.json`（若不存在）。

每个 Agent 还会维护一个 `mailbox.log`（JSONL），记录它“实际接收并进入感知”的所有消息。

## Conversations（会话聚合）

守护进程会按 `conversation_id` 将消息 ID 聚合到 `$UNICOS_CONVERSATIONS_DIR/{conv_id}.json`，用于后续按会话回溯完整上下文。

Agent 可通过工具 `conv_load` 读取某个 `conversation_id` 的消息列表并从上帝日志反查消息内容。

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
export UNICOS_CONVERSATIONS_DIR=/tmp/unicos-conversations
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
- `!conv_load <seed-or-8hex-id>`

工具返回会以普通消息形式回流到总线。

注意：工具调用的请求/响应细节默认不会广播到 topic（避免干扰频道）。Agent 会在内部消化工具结果后，再输出最终回复。

如果你希望强制 Agent 不直接发言（避免把模型原始输出直接广播），可以让 Agent 通过 `say` 工具发送消息：模型先工具调用（如 `bash/fs/net`），然后用 `say` 输出总结与关键结果。
