# Muxi

Muxi 是一个使用 Rust 编写的、以任务为中心的 Vim 模态终端编码助手。

项目当前处于 **0.1 前期开发阶段**。现有可运行版本提供：

- Ratatui/Crossterm 终端界面
- Anthropic Messages API 单轮文本流式请求
- Provider、模型和 Token 用量状态展示
- 类 Emacs 的 `Ctrl+X f` 文件打开快捷键
- `Alt+O`（macOS `Cmd+O`）Workspace Sidebar：多 workspace 列表、切换与持久化，添加时通过系统原生目录选择框选择文件夹，文件操作限定在当前 workspace 边界内
- 带 Vim `relativenumber` 的单文件 UTF-8 文本编辑 Buffer
- 基础任务状态机、SQLite 事件日志、内容寻址存储与安全文件读取原语

> 任务执行、工具调用、Git/Shell 操作、计划审批、验证、评审和会话恢复等能力仍在开发中，尚未全部接入主程序。

## 目录

- [系统要求](#系统要求)
- [快速开始](#快速开始)
- [配置大模型](#配置大模型)
- [界面与状态栏](#界面与状态栏)
- [快捷键总览](#快捷键总览)
- [文件 Buffer](#文件-buffer)
- [命令模式](#命令模式)
- [Workspace 结构](#workspace-结构)
- [开发与验证](#开发与验证)
- [当前限制](#当前限制)
- [路线图](#路线图)
- [许可证](#许可证)

## 系统要求

- Rust `1.92`
- Windows 11、Linux 或 macOS
- Windows 推荐使用 PowerShell 7 和支持现代终端控制序列的终端，例如 WezTerm 或 Windows Terminal
- 首次构建依赖需要网络访问
- 调用 Anthropic API 时需要可访问对应 API 地址

## 快速开始

### 构建

```sh
cargo build --workspace
```

### 运行

打开当前目录作为工作区：

```sh
cargo run -p muxi -- .
```

打开指定工作区：

```sh
cargo run -p muxi -- path/to/workspace
```

Windows PowerShell 示例：

```powershell
cargo run -p muxi -- E:\path\to\workspace
```

也可以直接运行构建产物：

```sh
./target/debug/muxi
```

Windows：

```powershell
.\target\debug\muxi.exe
```

### CLI 帮助

```sh
cargo run -p muxi -- --help
cargo run -p muxi -- --version
```

CLI 格式：

```text
muxi [WORKSPACE]
```

`WORKSPACE` 省略时默认为当前目录。

## 配置大模型

### 配置文件优先级

Muxi 使用找到的第一个配置文件：

1. 当前工作区下的 `muxi.toml`
2. Claude Code 的配置文件，例如 `C:\Users\Administrator\.claude\settings.json`，也就是 `cc switch` 写入的配置
3. Muxi 自己的全局配置文件

全局配置文件路径：

| 平台 | 优先路径 | 兼容回退 |
|---|---|---|
| Windows | `%USERPROFILE%\.muxi\config.toml` | `%APPDATA%\.muxi\config.toml` |
| Linux | `~/.muxi/config.toml` | `~/.config/.muxi/config.toml` |
| macOS | `~/.muxi/config.toml` | 系统配置目录下的 `.muxi/config.toml` |

工作区配置是**整文件覆盖**，不是字段合并。若工作区存在 `muxi.toml`，Muxi 不再读取 Claude Code 或全局配置；若该文件格式错误，程序会报错退出，不会回退到后面的配置。

如果没有工作区配置，Muxi 会优先读取 Claude Code 的 `settings.json`：

```json
{
  "env": {
    "ANTHROPIC_AUTH_TOKEN": "PROXY_MANAGED",
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:5000",
    "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-4-6"
  }
}
```

这和 `cc switch` 使用的是同一套配置。读取 Claude Code 配置时，Muxi 会使用 `Authorization: Bearer <ANTHROPIC_AUTH_TOKEN>` 认证，并把 `ANTHROPIC_BASE_URL` 当作 Anthropic-compatible 网关地址。

模型选择优先级为：非空 `ANTHROPIC_MODEL` → 非空 `ANTHROPIC_DEFAULT_SONNET_MODEL` → Muxi 内置 Sonnet 模型。CC Switch 的 `ANTHROPIC_DEFAULT_OPUS_MODEL`、`ANTHROPIC_DEFAULT_SONNET_MODEL`、`ANTHROPIC_DEFAULT_HAIKU_MODEL` 是并列的角色映射，并不表示当前选中了哪个角色；Muxi 当前默认使用 Sonnet 角色。对应的 `ANTHROPIC_DEFAULT_*_MODEL_NAME` 只用于界面显示，不会作为 API 请求中的模型 ID。

普通 Claude Code `settings.json` 如果没有完整的 `ANTHROPIC_AUTH_TOKEN` 和 `ANTHROPIC_BASE_URL`，Muxi 会继续查找自己的全局配置；只配置其中一项则会报告配置错误。

没有任何配置文件时，Muxi 使用内置 `mock` provider，不访问网络。

### Anthropic 配置示例

在工作区创建 `muxi.toml`，或创建 Muxi 全局配置 `%USERPROFILE%\.muxi\config.toml`：

```toml
[provider]
kind = "anthropic"
model = "claude-sonnet-5"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"
```

字段说明：

| 字段 | 是否必填 | 默认值 | 说明 |
|---|---:|---|---|
| `provider.kind` | 否 | `mock` | 可选值：`mock`、`anthropic` |
| `provider.model` | 否 | `claude-sonnet-5` | 请求使用的模型 ID |
| `provider.base_url` | 否 | `https://api.anthropic.com` | API 地址，可用于兼容网关或代理 |
| `provider.api_key_env` | 否 | `ANTHROPIC_API_KEY` | 保存 API Key 的环境变量名 |

配置使用严格字段校验。未知字段、错误的 `kind`、空模型名或空 API Key 都会导致启动失败并显示原因。

### 设置 API Key

API Key 只从环境变量读取，不应写入 TOML 或提交到 Git。

Windows PowerShell，仅当前终端生效：

```powershell
$env:ANTHROPIC_API_KEY = "sk-ant-..."
```

Windows，持久写入用户环境变量：

```powershell
setx ANTHROPIC_API_KEY "sk-ant-..."
```

`setx` 设置后需要重新打开终端。

Linux/macOS：

```sh
export ANTHROPIC_API_KEY="sk-ant-..."
```

如果使用自定义环境变量名：

```toml
[provider]
kind = "anthropic"
api_key_env = "MY_ANTHROPIC_KEY"
```

### Mock Provider

显式配置 mock：

```toml
[provider]
kind = "mock"
```

Mock 模式不会访问网络，主要用于开发和测试。默认 mock 没有预置文本响应，因此发送消息后通常不会产生模型文本。

## 界面与状态栏

Muxi 界面采用深色终端风格，自上而下分为：

1. **Header**：`✳ Muxi` 标识、模型与 Provider、当前 workspace 路径；有提示/警告时显示 `!! 消息` 行
2. **Workspace Sidebar**（可选，`Alt+O` 开关）：顶部搜索框（`/` 过滤，无占位文字）、workspace 列表；未选中项为整行底色的卡片（`●` 图标 + 名称 + 目录路径，路径过长以 `…` 截断），选中项不带底色
3. **聊天区**：用户消息显示为整行反色的 `> 消息` 栏，模型回复显示在下方
4. **Turn 状态行**：模型运行中显示 `✳ Spinning… (秒数 · ↓ tokens · still thinking)`，结束后显示 `↳ 输入↑ / 输出↓ tokens` 用量
5. **Composer 输入区**：顶部一条分隔线，`>` 提示符后输入（Command 模式为 `:`，文件提示为 `find file:`，添加 workspace 为 `workspace name:`）
6. **提示行**：`❯❯ 模式名` + 当前模式可用的按键提示，运行中追加 `· turn running`
7. **底部工具栏**：左侧功能 pill 按钮（`i compose`、`C-x f file`、`alt+o sidebar`、`:q quit`），右侧显示 workspace 路径与模型 pill

状态栏示例（模型运行中）：

```text
✳ Spinning… (12s · ↓ 300 tokens · still thinking)
❯❯ INSERT enter send · shift+enter newline · esc normal · turn running
```

模式包括：

| 模式 | 含义 |
|---|---|
| `NORMAL` | Vim 风格普通模式 |
| `INSERT` | 输入模型提示或编辑文件 |
| `COMMAND` | 输入 `:` 命令 |
| `FILE` | 输入需要打开的文件路径 |
| `SIDEBAR` | 焦点在左侧 Workspace Sidebar 列表上 |
| `SEARCH` | Sidebar 搜索框：输入过滤 workspace 列表 |
| `WORKSPACE` | 输入新 Workspace 的显示名称（`Enter` 后弹出系统目录选择框） |

编辑状态下使用终端原生的闪烁竖线光标；退出程序时会恢复用户原本的终端光标样式。

## 快捷键总览

### 全局快捷键

这些快捷键优先于当前模式处理：

| 快捷键 | 功能 |
|---|---|
| `Ctrl+C` | 直接退出 Muxi |
| `Ctrl+X`，然后按 `f` | 打开文件路径输入框 |
| `Ctrl+W`，然后按 `w` | 在两个 buffer（主窗口 ↔ Workspace Sidebar）之间切换焦点；Sidebar 隐藏时先打开 |
| `Alt+O`（macOS `Cmd+O`） | 打开/关闭 Workspace Sidebar |

`Ctrl+X f` 与 `Ctrl+W w` 是前缀键：先按住修饰键按第一个字母，松开后再按对应字母，不是同时按。前缀后按了其他键会取消前缀，该键仍会被当前焦点窗口处理。

`Alt+O` / `Cmd+O` 是全局快捷键，在任何模式下都生效。多数 macOS 终端不会把 `Cmd` 组合键转发给程序，此时可尝试 `Option+O`；Windows 与 Linux 使用 `Alt+O`。

`Ctrl+C` 当前会退出整个程序，不是取消当前模型请求。

### Workspace Sidebar

按 `Alt+O`、`t t` 或 `Ctrl+W w` 打开 Workspace Sidebar。Sidebar 与主窗口是两个 buffer：`Ctrl+W w` 在两者之间切换焦点（Vim `C-w w` 风格）；`Alt+O` / `t t` 开关侧栏——Sidebar 聚焦时再按 `t t` 同样会关闭它。Sidebar 列出所有已添加的 workspace（显示名称与目录路径），所有模型请求与文件操作只在**当前激活**的 workspace 下进行（Header 与状态栏显示激活路径）。

Sidebar 聚焦时的按键：

| 按键 | 功能 |
|---|---|
| `j` / `k`（或方向键） | 上下移动选择 |
| `t`，然后 `t` | 关闭 Workspace Sidebar（双键开关，等价 `Alt+O`） |
| `/` | 进入搜索框：输入即过滤列表，方向键选择，`Esc` 清空并返回 |
| `Enter` | 激活选中的 workspace |
| `a` | 添加 workspace：先输入显示名称，`Enter` 后弹出系统原生目录选择框选择文件夹（`Esc` 取消） |
| `d` | 从列表移除选中的 workspace |
| `Esc` | 退出 Sidebar 焦点（Sidebar 保持显示） |
| `Alt+O` | 关闭 Sidebar |

添加 workspace 的流程：

1. 按 `Alt+O` 打开 Sidebar，按 `a`
2. 输入 workspace 的显示名称（显示在 Sidebar 列表中），按 `Enter`
3. Muxi 暂时挂起终端界面，弹出系统原生目录选择框（Windows 使用 `FolderBrowserDialog`，macOS 使用 `choose folder`，Linux 使用 `zenity`/`kdialog`）
4. 选择项目目录后，Muxi 恢复界面，Sidebar 中显示刚输入的名称并自动激活该 workspace

规则：

- Workspace 列表持久化在 `SQLite` 数据库 `%USERPROFILE%\.muxi\muxi.db`（Linux/macOS 为 `~/.muxi/muxi.db`），按插入顺序存储 `name` + `path` 条目
- 旧版 `workspaces.toml`（`[[workspaces]]` 表数组或更早的纯路径列表）会在启动时自动导入数据库，然后改名为 `workspaces.toml.imported` 归档
- 搜索框按名称或路径的不区分大小写子串过滤；无匹配时 `Enter` 不会激活任何条目
- 启动时命令行传入的 workspace 会自动加入列表（名称取目录名）并激活
- 添加时所选路径必须是一个已存在的目录；重复添加相同目录会直接跳转到已有条目
- 模型请求运行期间（`busy`）锁定 workspace 切换、删除与激活，保证请求只在发起时所在的 workspace 内操作
- 切换 workspace 时会关闭当前打开的文件 Buffer（旧文件不在新 workspace 边界内）
- 至少保留一个 workspace，最后一个不可删除

### Normal 模式：聊天界面

| 按键 | 功能 |
|---|---|
| `i` | 进入 Insert 模式，编辑 Composer |
| `:` | 进入 Command 模式 |
| `t`，然后 `t` | 打开/关闭 Workspace Sidebar（Vim 双键风格，等价 `Alt+O`） |
| `Ctrl+W`，然后 `w` | 主窗口 ↔ Sidebar 切换焦点（Sidebar 隐藏时先打开） |
| `Ctrl+X f` | 打开文件 |
| `Alt+O` | 打开/关闭 Workspace Sidebar（进入 Sidebar 聚焦） |
| `q` | 无操作；退出必须使用 `:q` |
| `h` / `j` / `k` / `l` | 当前仅显示方向提示，尚未连接到聊天视图导航 |

### Insert 模式：Composer

| 按键 | 功能 |
|---|---|
| 普通字符 | 在 Composer 末尾输入文字 |
| `Backspace` | 删除最后一个字符或换行符 |
| `Shift+Enter` | 插入换行 |
| `Enter` | 发送当前消息 |
| `Esc` | 返回 Normal 模式 |

发送消息时：

- 空白内容不会发送
- 内容首尾空白会被移除
- 输入框和上一条回复会被清空
- 状态变为 `busy`
- 回复以流式增量显示在 Primary buffer 中
- 同一时间只允许运行一个模型请求
- 正在请求时再次发送会保留输入，并显示 `A turn is already running.`
- 发送后仍停留在 Insert 模式

### File 模式

按 `Ctrl+X f` 进入。

| 按键 | 功能 |
|---|---|
| 普通字符 | 输入文件路径 |
| `Backspace` | 删除路径末尾字符 |
| `Enter` | 打开文件 |
| `Esc` | 取消并返回 Normal 模式 |

路径可以是：

- 相对于当前 Workspace 的路径，例如 `README.md`
- 绝对路径，但必须位于当前 Workspace 内，否则会被拒绝（提示 `path escapes workspace`）

只能打开已存在且可按 UTF-8 解码的文本文件。

### Normal 模式：文件 Buffer

| 按键 | 功能 |
|---|---|
| `h` | 光标左移一个字符 |
| `j` | 光标下移一行 |
| `k` | 光标上移一行 |
| `l` | 光标右移一个字符 |
| `i` | 进入文件 Insert 模式 |
| `:` | 进入 Command 模式 |
| `Ctrl+X f` | 打开另一个文件并替换当前 Buffer |
| `t`，然后 `t` | 打开/关闭 Workspace Sidebar |
| `Ctrl+W`，然后 `w` | 主窗口 ↔ Sidebar 切换焦点 |
| `Alt+O` | 打开/关闭 Workspace Sidebar |

上下移动时，光标列会被限制在目标行实际长度内。

### Insert 模式：文件 Buffer

| 按键 | 功能 |
|---|---|
| 普通字符 | 在文件光标位置插入字符 |
| `Enter` | 在光标处拆分当前行并进入下一行 |
| `Backspace` | 删除前一个字符；行首时与上一行合并 |
| `Esc` | 返回 Normal 模式 |

当前文件编辑器支持 Unicode 字符索引和中文等宽字符的基本光标定位。

## 文件 Buffer

### 打开文件

例如打开项目 README：

1. 按 `Ctrl+X`
2. 松开后按 `f`
3. 输入 `README.md`
4. 按 `Enter`

打开后，Primary buffer 会显示文件路径和文本内容。

### Relative number

文件左侧使用类似 Vim `relativenumber` 的相对行号：

```text
2  第一行
1  第二行
0  当前光标行
1  第四行
2  第五行
```

当前行固定显示 `0`，其他行显示与当前行的距离。

### 保存和关闭

保存当前文件：

```text
:w
```

关闭当前文件 Buffer：

```text
:bd
```

保存并退出：

```text
:wq
```

> 当前没有 dirty 标记或未保存确认。`:bd`、`:q`、`Ctrl+C` 以及打开另一个文件都可能丢失未保存内容。

## 命令模式

Normal 模式按 `:` 进入。输入命令后按 `Enter` 执行，按 `Esc` 取消。

| 命令 | 功能 |
|---|---|
| `:q` | 退出 Muxi |
| `:quit` | 同 `:q` |
| `:w` | 保存当前文件 |
| `:write` | 同 `:w` |
| `:wq` | 保存当前文件后退出 |
| `:bd` | 关闭当前文件 Buffer |
| `:close` | 同 `:bd` |
| `:help` | 显示命令摘要 |
| `:task` | 显示任务视图占位提示；任务视图尚未实现 |

注意：

- 普通模式下直接按 `q` 不退出
- `:q` 不检查未保存修改
- `:bd` 不检查未保存修改
- `:wq` 当前即使保存失败也会继续退出
- 命令暂不支持参数，例如尚无 `:e path`

## Anthropic 请求行为

当前 Anthropic 适配器直接调用 Messages API：

```text
POST {base_url}/v1/messages
```

请求包含：

- 工作区/全局 Anthropic 配置使用 `x-api-key`；从 CC Switch 配置导入时使用 `Authorization: Bearer`
- `anthropic-version: 2023-06-01`
- 配置的模型 ID
- `stream: true`
- 固定 `max_tokens: 4096`
- 当前 Composer 内容作为唯一一条 `user` 消息

当前能处理的 SSE 事件：

- `message_start`
- 文本 `content_block_delta`
- `message_delta`
- `message_stop`
- `error`

SSE 解码支持 LF、CRLF、CR、任意网络 chunk 边界和跨 chunk UTF-8。`message_delta` 记录 Token 用量和 stop reason，只有后续 `message_stop` 才完成本轮；若连接提前结束、事件损坏或 provider 未发送终态，Muxi 会显示失败并恢复为 Idle，已经收到的部分回复会保留。`max_tokens`、`tool_use`、`pause_turn`、`refusal` 和上下文窗口耗尽也会显示对应原因。

HTTP 请求超时固定为 300 秒。

## Workspace 结构

```text
crates/
├── muxi/           # 可执行程序、CLI、配置加载与运行时组装
├── muxi-core/      # 任务领域模型、阶段状态机和确定性 reducer
├── muxi-provider/  # Provider 接口、MockProvider、Anthropic SSE 适配器
├── muxi-store/     # SQLite 存储：事件日志、workspace 持久化、内容寻址存储（CAS）
├── muxi-tools/     # Workspace 路径边界、安全文件读取和 BLAKE3 哈希
└── muxi-tui/       # Ratatui UI、模式输入、模型流式展示和文件编辑器
```

### `muxi`

- 解析 `muxi [WORKSPACE]`
- 加载工作区或全局配置
- 创建 Mock 或 Anthropic Provider
- 启动 TUI

### `muxi-core`

包含任务模型与阶段状态机，例如：

- Draft
- Analysis
- Planning
- AwaitingPlanApproval
- Executing
- Verifying
- Reviewing
- Completed
- WaitingForUser
- WaitingForPermission
- Paused
- Cancelled
- Failed
- Recovery

该状态机目前还没有完整接入 TUI 用户流程。

### `muxi-provider`

提供统一的流式接口和事件：

- Started
- TextDelta
- ThinkingDelta
- ToolCall
- Usage
- Finished

当前端到端可用的是文本流式回复和 Token 用量；Thinking、Tool use 和 Refusal 详情尚未完整接入 UI。

### `muxi-store`

包含：

- SQLite 事件追加和 reducer 重放
- `WorkspaceStore`：Sidebar workspace 列表的 SQLite 持久化（`~/.muxi/muxi.db`），含旧 `workspaces.toml` 导入解析
- 使用 BLAKE3 内容哈希的 CAS
- 临时文件后重命名的内容提交方式

Workspace 列表已由主程序使用；事件日志和 CAS 仍未接入，程序重启不会恢复 TUI 会话。

### `muxi-tools`

包含：

- Workspace 根路径规范化
- 拒绝绝对路径与工作区逃逸
- UTF-8/二进制文件读取
- BLAKE3 内容哈希

TUI 文件 Buffer 已通过 `muxi-tools::Workspace` 使用安全边界：`Ctrl+X f` 打开的相对路径在当前 Workspace 内解析，绝对路径必须位于当前 Workspace 内。

## 开发与验证

### 格式检查

```sh
cargo fmt --check
```

### Clippy

```sh
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

### 测试

```sh
cargo test --workspace --all-features
```

### Release 构建

```sh
cargo build --workspace --release
```

### 完整检查

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --release
```

CI 会在 Windows、Ubuntu 和 macOS 上运行检查。

真实 Anthropic API 请求不会进入默认测试套件，避免测试产生外部费用。

## 当前限制

### 模型交互

- 仅支持单轮请求，不携带历史消息
- 不支持 System prompt 配置
- 不支持通过 TOML 修改 `max_tokens`、timeout、temperature 等参数
- 没有请求重试或指数退避
- 没有工具调用循环
- 没有 Prompt caching
- 没有多模态输入
- 没有从 UI 单独取消当前请求的快捷键
- 同一时间只允许运行一个请求

### 文件编辑器

- 一次只能打开一个 Buffer
- 仅支持 UTF-8 文本文件
- 无撤销/重做
- 无搜索、复制粘贴、Visual 模式、语法高亮和自动缩进
- 尚未实现 `dd`、`yy`、`p`、`w`、`b` 等完整 Vim 操作
- 长文件没有滚动视口，超出可视范围的内容会被裁剪
- 无未保存状态和关闭确认
- 打开新文件会替换旧 Buffer
- 保存会统一用 `\n` 重写文本，不能可靠保留原始 CRLF 和文件末尾换行
- Sidebar 列表没有滚动，workspace 数量多时超出部分会被裁剪

### 任务与存储

- `:task` 仍是占位命令
- 任务状态机尚未接入 TUI
- SQLite 事件日志和 CAS 尚未由主程序使用
- 程序重启后 workspace 列表从 `muxi.db` 恢复，但消息、打开文件、光标和任务状态不恢复

### 0.1 明确不包含

- LSP
- MCP
- 插件运行时
- Headless 模式
- 多模态输入
- 完整 Vim 兼容

## 路线图

后续计划包括：

- 将任务状态机接入 TUI
- 多轮模型对话和会话恢复
- Provider 工具调用循环
- Workspace 安全读写与 Hash Guard 编辑
- Shell 与 Git 检查
- 权限确认和计划审批
- 文件滚动、多个 Buffer、Dirty 状态与撤销/重做
- 更完整的 Vim 操作
- 验证和评审流程

## 许可证

本项目可任选以下许可证使用：

- Apache License 2.0
- MIT License
