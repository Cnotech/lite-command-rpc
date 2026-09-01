# Lite Command RPC

面向 Windows 的轻量级 HTTP 命令执行服务，为 Agent、自动化工具和 Windows PE 排障环境提供统一的远程操作入口。

LCR 以单个 `lcr.exe` 运行，无额外运行时依赖，支持同步、流式和异步命令执行，以及屏幕、窗口、键鼠和文件操作。

> [!WARNING]
> 对外暴露 LCR 控制端口是非常危险的，请确保仅在可信网络中使用。

> [!NOTE]
> 本项目仅支持 Windows 平台，主要面向 Windows PE 和 Windows 自动化场景。

## 目录

- [功能特性](#功能特性)
- [快速开始](#快速开始)
- [命令行选项](#命令行选项)
- [安全配置](#安全配置)
- [API 参考](#api-参考)
- [从源码构建](#从源码构建)

## 功能特性

- 同步执行命令并返回完整结果
- 通过 NDJSON 实时返回 stdout 和 stderr
- 异步启动、查询和终止命令
- 超时或主动终止时清理整个进程树
- 截取主屏幕、枚举窗口并模拟键盘鼠标输入
- 流式上传和下载文件
- 通过工作目录、命令白名单和 UAC 开关收敛权限
- 配置文件热重载
- 单文件分发，无额外运行时依赖

## 快速开始

### 1. 下载

从 [GitHub Releases](https://github.com/Cnotech/lite-command-rpc/releases) 下载最新的 Windows ZIP 压缩包并解压。

### 2. 启动服务

```powershell
.\lcr.exe --listen 127.0.0.1:9527
```

`lcr.exe serve` 与不带子命令启动的行为相同。若不指定 `--listen`，服务默认监听 `0.0.0.0:9527`。

### 3. 发送请求

```powershell
curl -X POST http://127.0.0.1:9527/exec `
  -H "Content-Type: application/json" `
  --data-raw '{"program":"cmd.exe","args":["/d","/s","/c","ver"]}'
```

响应示例：

```json
{
  "ok": true,
  "exit_code": 0,
  "stdout": "Microsoft Windows ...",
  "stderr": "",
  "timed_out": false,
  "error": null
}
```

## 命令行选项

```text
lcr.exe [OPTIONS] [COMMAND]

Commands:
  serve  （可省略）启动 HTTP 服务

Options:
  --listen <IP:PORT>   监听地址，默认 0.0.0.0:9527
  --config <PATH>      显式指定 TOML 配置文件
  --log-level <LEVEL>  日志级别，默认 info
  -h, --help           显示帮助
  -V, --version        显示版本
```

例如仅监听 IPv6 本地回环地址：

```powershell
.\lcr.exe --listen "[::1]:9527"
```

## 安全配置

建议从仓库中的 [`lcr.toml.example`](./lcr.toml.example) 复制配置，并至少设置 `work_dir` 和 `command_allowlist`：

```toml
# 相对路径以 lcr.toml 所在目录为基准，目录必须已经存在。
work_dir = "workspace"

# 普通字符串按前缀匹配；/…/ 中的内容按正则表达式匹配。
# 两种规则均忽略 ASCII 大小写。
command_allowlist = [
  '/^git(\.exe)? "status"$/',
  '/^cargo(\.exe)? "(check|test)"/',
  "WimBuilder.cmd",
]

# 是否允许请求通过 require_admin = true 进行 UAC 提权；默认关闭。
allow_elevation = false
```

### 配置文件查找与热重载

启动时按以下顺序选择配置：

1. `--config PATH` 显式指定的文件
2. 当前工作目录中的 `lcr.toml`
3. `lcr.exe` 同目录中的 `lcr.toml`

自动查找时，只有文件不存在才会继续查找下一位置。文件无法读取、字段未知或内容无效时，服务会拒绝启动；两个默认位置都没有配置文件时，则以不受配置策略限制的兼容模式运行。

成功加载配置后，LCR 会监视该文件。修改或原子替换文件时，新配置会先经过验证，再通过重启 HTTP worker 生效。无效配置或文件被删除不会覆盖最后一次有效配置。worker 重启会中断正在处理的 HTTP 请求和非 detached 命令。

启动时未找到默认配置文件，则本次运行不会监听之后新建的配置文件。

### 限制工作目录

`work_dir` 可以是单个目录：

```toml
work_dir = "D:\\Workspace"
```

此时：

- 未传 `cwd` 的命令默认在该目录运行。
- 相对 `cwd`、上传路径和下载路径均以该目录为根。
- 命令工作目录、临时脚本和文件传输路径不能越过该目录边界。

也可以允许多个目录：

```toml
work_dir = ["D:\\WorkspaceA", "D:\\WorkspaceB"]
```

数组模式没有唯一默认根目录，因此每个命令请求都必须提供位于允许目录内的绝对 `cwd`，上传和下载也必须使用绝对路径。

Windows 上，LCR 会规范化路径、拒绝残留的重解析点，并在操作期间保持目录句柄；下载时还会根据已打开的文件句柄复核源路径，以降低通过符号链接、目录联接或并发替换越界的风险。

> [!IMPORTANT]
> `work_dir` 是 LCR 的请求策略，不是 Windows 进程沙箱。获准执行的程序仍可按当前用户权限访问其他路径。需要强隔离时，请配合独立低权限账户、ACL 或系统级沙箱。

### 限制可执行程序

`command_allowlist` 非空时，执行接口进入严格白名单模式：

- 只接受 `program` + `args`，拒绝 `command`。
- 拒绝 CMD、PowerShell、Python、Node 等命令或脚本解释器，避免通过解释器绕过规则。
- 匹配文本由程序名和全部参数组成；参数按 JSON 字符串格式加引号。
- 普通规则执行忽略大小写的前缀匹配，`/…/` 规则执行忽略大小写的正则匹配。
- 被策略拒绝的 `/exec`、`/exec/stream` 和 `/spawn` 请求返回 HTTP `403 Forbidden`。

例如以下请求的匹配文本为 `git.exe "status"`：

```json
{
  "program": "git.exe",
  "args": ["status"]
}
```

正则按 UTF-8 字节匹配，仅支持 ASCII 字符类和 ASCII 大小写匹配；`.*` 可覆盖 Unicode 参数，但 Unicode 属性、Unicode 字符类和 Unicode 大小写折叠不受支持。

同时设置 `work_dir` 后，可以按文件名运行已验证 `cwd` 中的 `.cmd` 或 `.bat` 文件：

```toml
work_dir = "C:\\Projects\\WimBuilder"
command_allowlist = ["WimBuilder.cmd"]
```

```json
{
  "program": "WimBuilder.cmd",
  "args": ["build"]
}
```

### 管理员权限

请求中的 `require_admin: true` 只有在服务端显式配置 `allow_elevation = true` 后才生效。LCR 未提升运行时会显示 UAC 确认框，并由提升权限的辅助进程执行命令。

```json
{
  "program": "WimBuilder.cmd",
  "require_admin": true
}
```

同一时间只允许一个待确认的 UAC 请求。用户取消时接口返回 `administrator elevation was cancelled`。提升执行仍受超时和进程树终止策略约束；stdout 和 stderr 各最多缓冲 8 MiB，`/exec/stream` 会在提升权限的辅助进程结束后统一回传输出。

## API 参考

所有接口均使用 `POST`。

| 接口 | 说明 | 请求体 | 成功响应 |
| --- | --- | --- | --- |
| `/exec` | 执行命令并等待完成 | JSON | JSON |
| `/exec/stream` | 流式执行命令 | JSON | NDJSON |
| `/spawn` | 异步启动命令 | JSON | JSON |
| `/spawn/result` | 查询异步任务及新增输出 | JSON | JSON |
| `/spawn/terminate` | 终止异步任务及其进程树 | JSON | JSON |
| `/screenshot` | 截取主屏幕 | 空 | PNG |
| `/windows` | 枚举顶级窗口 | 空 | JSON |
| `/control` | 聚焦窗口或模拟键鼠输入 | JSON | JSON |
| `/download` | 下载文件 | JSON | 二进制 |
| `/upload` | 上传文件 | 二进制 | JSON |

### 执行请求

`/exec`、`/exec/stream` 和 `/spawn` 使用相同的请求结构。

| 字段 | 必填 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `command` | 二选一 | — | 交给解释器执行的命令或脚本；不能与 `program` 同时使用 |
| `program` | 二选一 | — | 直接启动的程序名或 Windows 绝对路径 |
| `args` | 否 | `[]` | `program` 的参数数组 |
| `cwd` | 否 | 服务进程目录或配置的默认目录 | 命令工作目录 |
| `timeout` | 否 | `300000` | 超时时间，单位为毫秒 |
| `interpreter` | 否 | `cmd` | `cmd`、`pwsh` 或解释器的 Windows 绝对路径 |
| `script_mode` | 否 | `auto` | `auto`、`inline` 或 `file` |
| `detached` | 否 | `false` | 包装脚本退出后是否允许子进程继续运行 |
| `require_admin` | 否 | `false` | 请求通过 UAC 提升权限 |
| `output_encoding` | 否 | `utf8` | `utf8`、`oem` 或 `ansi` |

路径或参数包含 Unicode 字符时，优先使用 `program` 和 `args`，避免经过命令解释器的代码页和引号解析：

```json
{
  "program": "X:\\Windows\\System32\\notepad.exe",
  "args": ["C:\\资料\\说明.txt"]
}
```

使用 `command` 时，解释器调用方式如下：

- `cmd`：通过 `cmd.exe /d /s /c` 执行，并将代码页切换为 UTF-8。
- `pwsh`：通过 `pwsh.exe -NoLogo -NoProfile -NonInteractive -Command` 执行，需要 PowerShell 7。
- 绝对路径：`cmd.exe`、`pwsh.exe` 和 `powershell.exe` 使用各自参数，其他解释器通过 `<绝对路径> -c <command>` 执行。

自定义解释器必须使用绝对路径，路径包含空格时无需额外加引号：

```json
{
  "command": "print('hello')",
  "interpreter": "C:\\Python313\\python.exe"
}
```

#### 输出编码

默认的 `utf8` 适用于 UTF-8 输出。传统程序按系统代码页输出时，可选择 `oem`（`GetOEMCP`）或 `ansi`（`GetACP`）。流式接口会保留跨数据块的多字节字符。

#### 多行脚本

`script_mode` 控制脚本如何交给解释器：

- `auto`：包含换行符时使用临时文件，否则直接执行。
- `inline`：始终直接传给解释器。
- `file`：始终写入临时文件后执行。

```json
{
  "command": "@echo off\r\nset NAME=Lite Command RPC\r\necho %NAME%",
  "interpreter": "cmd",
  "script_mode": "auto"
}
```

临时脚本会在命令结束、超时或启动失败后删除。CMD 脚本使用 `.cmd` 和 UTF-8 代码页，PowerShell 脚本使用带 UTF-8 BOM 的 `.ps1`。

#### Detached 模式

`detached: true` 适用于包装脚本启动 GUI 程序的场景。该模式将 stdout 和 stderr 重定向到空设备，因此响应中的输出为空。包装脚本运行期间仍受超时和进程树终止控制；包装脚本正常退出后，其子进程不再由该会话跟踪。

如果 LCR 本身位于外层 Job Object，子进程能否脱离还取决于外层是否允许 breakaway。默认的 `false` 会保留输出捕获和严格的进程树清理行为。

### 同步执行：`/exec`

```powershell
curl -X POST http://127.0.0.1:9527/exec `
  -H "Content-Type: application/json" `
  --data-raw '{"command":"tasklist","timeout":300000,"output_encoding":"utf8"}'
```

```json
{
  "ok": true,
  "exit_code": 0,
  "stdout": "...",
  "stderr": "",
  "timed_out": false,
  "error": null
}
```

### 流式执行：`/exec/stream`

响应使用 HTTP Chunked 传输，内容类型为 NDJSON，每行是一个独立事件：

```powershell
curl --no-buffer -X POST http://127.0.0.1:9527/exec/stream `
  -H "Content-Type: application/json" `
  --data-raw '{"command":"ping 127.0.0.1 -n 4"}'
```

```jsonl
{"type":"stdout","data":"hello\r\n"}
{"type":"stderr","data":"warning\r\n"}
{"type":"exit","exit_code":0}
```

超时时最后一个事件为 `{"type":"timeout","timeout":300000}`；启动失败时返回 `error` 事件。

### 异步执行：`/spawn`

`/spawn` 成功启动命令后立即返回 `202 Accepted`：

```powershell
curl -X POST http://127.0.0.1:9527/spawn `
  -H "Content-Type: application/json" `
  --data-raw '{"command":"ping 127.0.0.1 -n 10"}'
```

```json
{
  "session_id": "1234-1",
  "pid": 5678,
  "status": "running"
}
```

使用 `/spawn/result` 查询状态。轮询时传回上一次响应中的 `stdout_next_offset` 和 `stderr_next_offset`，即可只获取新增输出：

```powershell
curl -X POST http://127.0.0.1:9527/spawn/result `
  -H "Content-Type: application/json" `
  --data-raw '{"session_id":"1234-1","stdout_offset":0,"stderr_offset":0}'
```

```json
{
  "session_id": "1234-1",
  "pid": 5678,
  "status": "exited",
  "exit_code": 0,
  "stdout_offset": 0,
  "stdout_next_offset": 6,
  "stdout_complete": true,
  "stderr_offset": 0,
  "stderr_next_offset": 0,
  "stderr_complete": true,
  "stdout": "done\r\n",
  "stderr": "",
  "stdout_truncated": false,
  "stderr_truncated": false,
  "error": null
}
```

状态可能为 `starting`、`running`、`terminating`、`terminated`、`exited`、`timed_out` 或 `failed`。

异步会话限制：

- 每个 stdout/stderr 最多保留 8 MiB，所有会话合计最多保留 64 MiB。
- 单次查询每个输出流最多返回 1 MiB；`*_complete` 为 `false` 时应继续查询。
- 已完成会话通常保留 30 分钟，最多保存 128 个会话。
- 达到上限时优先淘汰最早完成的会话；128 个会话均在运行时拒绝新任务。

使用 `/spawn/terminate` 终止任务及其 Job Object 进程树：

```powershell
curl -X POST http://127.0.0.1:9527/spawn/terminate `
  -H "Content-Type: application/json" `
  --data-raw '{"session_id":"1234-1"}'
```

响应结构与 `/spawn/result` 相同。主动终止后的状态为 `terminated`，`exit_code` 为 `null`；重复终止已结束的任务不会改变结果。

### 截取屏幕：`/screenshot`

截取当前主屏幕并直接返回 `image/png`：

```powershell
curl -X POST http://127.0.0.1:9527/screenshot --output screenshot.png
```

### 枚举窗口：`/windows`

```powershell
curl -X POST http://127.0.0.1:9527/windows
```

```json
{
  "foreground_hwnd": "0xA12BC",
  "windows": [
    {
      "hwnd": "0xA12BC",
      "pid": 1234,
      "thread_id": 5678,
      "title": "Command Prompt",
      "rect": {
        "left": 100,
        "top": 80,
        "right": 1100,
        "bottom": 780,
        "width": 1000,
        "height": 700
      },
      "top_level": true,
      "foreground": true,
      "topmost": false,
      "visible": true,
      "enabled": true,
      "minimized": false,
      "maximized": false
    }
  ]
}
```

`hwnd` 使用十六进制字符串表示，以避免客户端语言的整数精度问题。

### 控制窗口与输入：`/control`

操作按数组顺序执行。同一时间只处理一个控制请求，避免不同请求的键鼠动作交错。相邻动作默认间隔 50 毫秒，可通过 `delay` 修改：

```json
{
  "delay": 100,
  "actions": [
    { "type": "focus_window", "hwnd": "0xA12BC" },
    { "type": "keyboard", "key": "G" },
    { "type": "text", "text": "hello 世界" },
    { "type": "mouse_move", "x": 500, "y": 300 },
    { "type": "mouse_click", "button": "left" },
    { "type": "mouse_wheel", "delta": -120 }
  ]
}
```

| `type` | 字段 | 说明 |
| --- | --- | --- |
| `focus_window` | `hwnd` | 聚焦窗口；接受十六进制字符串、十进制字符串或整数 |
| `keyboard` | `key`, `state` | 按键；`state` 为 `down`、`up` 或 `press`，默认 `press` |
| `text` | `text` | 使用 Unicode 键盘事件输入文本 |
| `mouse_move` | `x`, `y` | 移动到主屏幕绝对坐标 |
| `mouse_button` | `button`, `state` | 鼠标按键操作 |
| `mouse_click` | `button` | 单击；默认 `left` |
| `mouse_wheel` | `delta` | 滚轮增量，通常以正负 120 为一格 |

`button` 支持 `left`、`right` 和 `middle`。`keyboard.key` 支持单个 ASCII 字母或数字、`F1`–`F24`、十六进制虚拟键码，以及常用的控制键、方向键和导航键。

限制：

- 每个请求最多 256 个动作。
- 单个 `text` 最多 4096 个 UTF-16 代码单元。
- 单次动作间隔最大 5000 毫秒，累计间隔不超过 30000 毫秒。
- `delay_ms` 是 `delay` 的兼容别名。

Windows 可能阻止后台进程聚焦窗口，或因权限级别不同而拒绝输入注入。失败时返回 `409 Conflict`，并包含失败动作索引和已完成动作数量；已执行动作不会回滚。

### 下载文件：`/download`

```powershell
curl -X POST http://127.0.0.1:9527/download `
  -H "Content-Type: application/json" `
  --data-raw '{"path":"D:\\Desktop\\test.7z"}' `
  --output test.7z
```

成功响应的正文即为文件内容。

### 上传文件：`/upload`

请求体直接传输文件内容，通过 `X-File-Path` 指定目标路径：

```powershell
curl -X POST http://127.0.0.1:9527/upload `
  -H "Content-Type: application/octet-stream" `
  -H "X-File-Path: D:\Desktop\uploaded.7z" `
  --data-binary "@D:\Download\source.7z"
```

上传采用流式写入，不会将整个文件加载到内存。目标已存在时返回 `409 Conflict`，不会覆盖原文件。提交时优先使用硬链接；在不支持硬链接的 WinPE RAM 磁盘上自动回退到不覆盖目标的文件复制。

```json
{
  "ok": true,
  "path": "D:\\Desktop\\uploaded.7z",
  "bytes": 123456
}
```

## 从源码构建

需要 Windows 和稳定版 Rust 工具链。

```powershell
cargo build --release
```

产物位于 `target\release\lcr.exe`。Release 配置针对单文件体积启用了尺寸优化、LTO、符号剥离和 panic abort，因此链接时间会长于普通开发构建。

提交更改前请运行：

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

端到端测试脚本位于 [`tests/e2e.ps1`](./tests/e2e.ps1)。
