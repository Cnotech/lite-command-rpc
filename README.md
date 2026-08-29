# Lite Command RPC

Lite Command RPC 是一个面向 Windows 的轻量级 HTTP 命令执行服务，可为 Agent 或自动化工具提供远程执行命令、上传文件和下载文件的简单入口。

> [!IMPORTANT]
> 本项目专为 Windows PE 场景下的问题排查设计，仅适配 Windows 平台。

## 特性

- 普通或流式或异步执行 Windows 命令
- 屏幕截图
- 窗口枚举
- 模拟键盘和鼠标输入
- 上传和下载文件
- 单文件运行，无额外运行时依赖，轻量级实现

## 下载
从 [GitHub Releases](https://github.com/Cnotech/lite-command-rpc/releases) 下载最新的 Windows ZIP 压缩包

## 使用

解压后运行：

```powershell
.\lcr.exe
```

服务默认监听所有网络接口的 `9527` 端口，可通过 `--listen` 修改监听地址，例如仅允许本机访问：

```powershell
.\lcr.exe --listen 127.0.0.1:9527
.\lcr.exe --listen [::1]:9527
```

也可以使用 `.\lcr.exe serve` 显式启动服务；其行为与不带参数运行相同。

> 服务目前不包含身份认证。请仅在可信网络中使用，或通过防火墙、反向代理等方式限制访问。

## 从源码构建

需要在 Windows 上安装稳定版 Rust 工具链：

```powershell
cargo build -r
```

构建产物位于：

```text
target\release\lcr.exe
```

## API

所有接口均使用 `POST` 方法。

| 接口 | 说明 | 请求类型 |
| --- | --- | --- |
| `/exec` | 执行命令并一次性返回结果 | JSON |
| `/exec/stream` | 执行命令并持续返回输出 | JSON |
| `/spawn` | 异步启动命令，立即返回会话 ID 和 PID | JSON |
| `/spawn/result` | 查询异步命令的状态和输出 | JSON |
| `/spawn/terminate` | 终止异步命令并返回最终结果 | JSON |
| `/screenshot` | 截取主屏幕并返回 PNG | 空请求体 |
| `/windows` | 枚举当前桌面的顶级窗口 | 空请求体 |
| `/control` | 聚焦窗口或模拟键盘、鼠标输入 | JSON |
| `/download` | 下载指定文件 | JSON |
| `/upload` | 上传文件到指定路径 | 二进制 |

### 执行命令

请求示例：

```powershell
curl -X POST http://127.0.0.1:9527/exec `
  -H "Content-Type: application/json" `
  --data-raw '{"command":"tasklist","cwd":"D:\\Desktop","timeout":300000,"interpreter":"cmd","script_mode":"auto"}'
```

字段说明：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `command` | 是 | 交给所选解释器执行的脚本或命令 |
| `cwd` | 否 | 命令的工作目录 |
| `timeout` | 否 | 超时时间，单位为毫秒，默认 300000（5 分钟） |
| `interpreter` | 否 | 脚本解释器，默认为 `cmd`；可设为 `cmd`、`pwsh` 或解释器的 Windows 绝对路径 |
| `script_mode` | 否 | 脚本执行方式，可设为 `auto`、`inline` 或 `file`，默认为 `auto` |
| `detached` | 否 | 是否允许包装脚本正常退出后其子进程继续运行，默认为 `false` |

解释器的调用方式如下：

- `cmd`：通过 `cmd.exe /d /s /c` 执行，并将代码页切换为 UTF-8。
- `pwsh`：通过 `pwsh.exe -NoLogo -NoProfile -NonInteractive -Command` 执行，需要系统已安装 PowerShell 7。
- 绝对路径：若文件名为 `cmd.exe`、`pwsh.exe` 或 `powershell.exe`，使用对应参数；其他解释器通过 `<绝对路径> -c <command>` 执行。

例如使用指定位置的 Python：

```json
{
  "command": "print('hello')",
  "interpreter": "C:\\Python313\\python.exe"
}
```

相对路径不受支持。绝对路径中包含空格时无需额外添加引号。

#### 多行脚本

`script_mode` 控制命令是直接传给解释器，还是先写入临时脚本文件：

- `auto`：检测到 `command` 中包含换行符时使用临时文件，否则直接执行。
- `inline`：始终直接传给解释器。
- `file`：始终写入临时文件后执行。

例如执行多行 CMD 脚本：

```json
{
  "command": "@echo off\r\nset NAME=Lite Command RPC\r\necho %NAME%",
  "interpreter": "cmd",
  "script_mode": "auto"
}
```

CMD 临时脚本使用 `.cmd` 扩展名并切换至 UTF-8 代码页，PowerShell 临时脚本使用带 UTF-8 BOM 的 `.ps1` 文件，其他解释器使用通用脚本文件。临时文件会在执行结束、超时或启动失败后自动删除。

`detached: true` 适用于由 CMD 或 PowerShell 包装脚本启动 GUI 程序的场景。为了避免 GUI 子进程继承输出管道并阻止包装脚本会话结束，detached 模式会将 stdout/stderr 重定向到空设备，因此响应中的两个输出字段为空。包装脚本仍会被加入 Job Object，所以在包装脚本运行期间，超时和 `/spawn/terminate` 仍会终止其进程树；包装脚本正常结束、会话进入 `exited` 后，已经启动的子进程不会因为 Job Object 关闭而被结束，也不再由该会话跟踪或终止。默认值 `false` 保持输出捕获和严格的进程树清理行为。

响应示例：

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

### 流式执行命令

`POST /exec/stream` 的请求体与 `/exec` 相同。响应使用 HTTP Chunked 传输，内容类型为 NDJSON，每行是一个独立事件：

```powershell
curl --no-buffer -X POST http://127.0.0.1:9527/exec/stream `
  -H "Content-Type: application/json" `
  --data-raw '{"command":"ping 127.0.0.1 -n 4","interpreter":"cmd"}'
```

响应示例：

```jsonl
{"type":"stdout","data":"hello\r\n"}
{"type":"stderr","data":"warning\r\n"}
{"type":"exit","exit_code":0}
```

命令超时时，最后一个事件为：

```json
{"type":"timeout","timeout":300000}
```

命令无法启动时会返回 `error` 事件。

### 异步执行命令

`POST /spawn` 的请求体与 `/exec` 相同。命令成功启动后立即返回 `202 Accepted`，不会等待命令执行完成：

```powershell
curl -X POST http://127.0.0.1:9527/spawn `
  -H "Content-Type: application/json" `
  --data-raw '{"command":"ping 127.0.0.1 -n 10","interpreter":"cmd"}'
```

响应示例：

```json
{
  "session_id": "1234-1",
  "pid": 5678,
  "status": "running"
}
```

使用 `POST /spawn/result` 查询运行状态和当前已经收集到的输出：

```powershell
curl -X POST http://127.0.0.1:9527/spawn/result `
  -H "Content-Type: application/json" `
  --data-raw '{"session_id":"1234-1","stdout_offset":0,"stderr_offset":0}'
```

响应示例：

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

状态可能为 `starting`、`running`、`terminating`、`terminated`、`exited`、`timed_out` 或 `failed`。`stdout_offset` 和 `stderr_offset` 是可选的 UTF-8 字节偏移量，默认为 0；持续轮询时将上次返回的 `*_next_offset` 传入，即可只获取新增输出。每个流单次最多返回 1 MiB；若 `*_complete` 为 `false`，继续使用新的 `*_next_offset` 查询剩余内容。

每个 stdout/stderr 最多保留 8 MiB，所有异步会话合计最多保留 64 MiB；超过限制时对应的 `*_truncated` 为 `true`。完成的会话通常保留 30 分钟，服务同时最多保存 128 个会话；达到会话上限时会优先淘汰最早完成的结果，只有 128 个会话都仍在运行时才拒绝新任务。异步命令必须成功加入 Job Object 才会报告启动成功，服务退出或命令超时时会终止对应进程树。

使用 `POST /spawn/terminate` 主动终止异步任务。请求字段与 `/spawn/result` 相同，至少提供 `session_id`：

```powershell
curl -X POST http://127.0.0.1:9527/spawn/terminate `
  -H "Content-Type: application/json" `
  --data-raw '{"session_id":"1234-1"}'
```

接口会终止该任务的 Job Object 进程树，等待状态收敛后返回与 `/spawn/result` 相同的结果结构。主动结束的任务状态为 `terminated`、`exit_code` 为 `null`。对已经结束的任务重复调用时不会改变结果，会直接返回现有最终状态。

### 截取屏幕

`POST /screenshot` 截取当前主屏幕，直接返回 `image/png` 二进制数据。

```powershell
curl -X POST http://127.0.0.1:9527/screenshot `
  --output screenshot.png
```

### 枚举窗口

`POST /windows` 返回当前输入桌面上的窗口信息：

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

`hwnd` 使用十六进制字符串表示，避免客户端语言的整数精度问题。

请求示例：

```powershell
curl -X POST http://127.0.0.1:9527/windows
```

### 控制窗口和输入

`POST /control` 在当前输入桌面按数组顺序执行操作。同一时间只执行一个控制请求，以避免不同请求的键鼠动作交错。相邻动作之间默认等待 50 毫秒，减少聚焦后紧接着输入时被 Windows 丢弃的概率；可通过顶层 `delay` 字段配置毫秒数，设为 0 可关闭等待，单次间隔最大为 5000，整个请求的累计动作间延迟不得超过 30000 毫秒：

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

请求示例：

```powershell
curl -X POST http://127.0.0.1:9527/control `
  -H "Content-Type: application/json" `
  --data-raw '{"delay":100,"actions":[{"type":"focus_window","hwnd":"0xA12BC"},{"type":"keyboard","key":"G"},{"type":"text","text":"hello 世界"},{"type":"mouse_move","x":500,"y":300},{"type":"mouse_click","button":"left"},{"type":"mouse_wheel","delta":-120}]}'
```

`delay_ms` 可作为 `delay` 的兼容别名。延迟只发生在两个动作之间，单个动作或最后一个动作后不会额外等待。

支持的操作：

| `type` | 字段 | 说明 |
| --- | --- | --- |
| `focus_window` | `hwnd` | 聚焦窗口；接受十六进制字符串、十进制字符串或整数 |
| `keyboard` | `key`, `state` | 按键操作；`state` 可为 `down`、`up`、`press`，默认 `press` |
| `text` | `text` | 使用 Unicode 键盘事件输入文本 |
| `mouse_move` | `x`, `y` | 移动到主屏幕绝对坐标 |
| `mouse_button` | `button`, `state` | 鼠标按键操作 |
| `mouse_click` | `button` | 单击，`button` 默认为 `left` |
| `mouse_wheel` | `delta` | 滚轮增量，通常以正负 120 为一格 |

`button` 可为 `left`、`right` 或 `middle`。`keyboard.key` 支持单个 ASCII 字母/数字、`F1`–`F24`、十六进制虚拟键码，以及 `enter`、`tab`、`escape`、`space`、`backspace`、`ctrl`、`shift`、`alt`、`win`、方向键、`home`、`end`、`pageup`、`pagedown`、`insert` 和 `delete`。

Windows 可能拒绝后台进程强制聚焦某些窗口，也可能因为权限级别阻止输入注入。发生失败时接口返回 `409 Conflict`，响应包含失败动作的索引和已完成动作数量；此前已经成功执行的动作不会回滚。

每个请求最多包含 256 个动作，单个 `text` 动作最多包含 4096 个 UTF-16 代码单元。

### 下载文件

请求示例：

```powershell
curl -X POST http://127.0.0.1:9527/download `
  -H "Content-Type: application/json" `
  --data-raw '{"path":"D:\\Desktop\\test.7z"}' `
  --output test.7z
```

成功后响应体即为文件的二进制内容。

### 上传文件

请求体直接传输文件内容，通过 `X-File-Path` 请求头指定目标路径：

```powershell
curl -X POST http://127.0.0.1:9527/upload `
  -H "Content-Type: application/octet-stream" `
  -H "X-File-Path: D:\Desktop\uploaded.7z" `
  --data-binary "@D:\Download\source.7z"
```

上传过程采用流式写入，不会将整个文件加载到内存。目标文件已存在时返回 `409 Conflict`，不会覆盖原文件。提交文件时优先使用硬链接；对于不支持硬链接的 WinPE RAM 磁盘，会自动回退到不覆盖目标的文件复制。

成功响应示例：

```json
{
  "ok": true,
  "path": "D:\\Desktop\\uploaded.7z",
  "bytes": 123456
}
```
