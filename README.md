# Lite Command RPC

Lite Command RPC 是一个面向 Windows 的轻量级 HTTP 命令执行服务，可为 Agent 或自动化工具提供远程执行命令、上传文件和下载文件的简单入口。

> [!IMPORTANT]
> 本项目仅适配 Windows 平台。命令通过 `cmd.exe` 执行，进程树终止等能力也依赖 Windows API。

## 功能

- 普通或流式执行 Windows 命令
- 支持设置工作目录和执行超时时间
- 超时后终止整个命令进程树
- 上传和下载文件
- 使用 UTF-8 代码页执行命令，便于正确返回中文输出
- 单文件部署，无额外运行时依赖

## 获取与运行

从 GitHub Releases 下载最新的 Windows ZIP 压缩包，解压后运行：

```powershell
.\lcr.exe
```

服务默认监听所有网络接口的 `9527` 端口：

```text
http://0.0.0.0:9527
```

> 服务目前不包含身份认证。请仅在可信网络中使用，或通过防火墙、反向代理等方式限制访问。

## 从源码构建

需要在 Windows 上安装稳定版 Rust 工具链：

```powershell
cargo build --release --locked
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
| `/download` | 下载指定文件 | JSON |
| `/upload` | 上传文件到指定路径 | 二进制 |

### 执行命令

请求：

```http
POST /exec
Content-Type: application/json
```

```json
{
  "command": "tasklist",
  "cwd": "D:\\Desktop",
  "timeout": 300000,
  "interpreter": "cmd"
}
```

字段说明：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `command` | 是 | 交给所选解释器执行的脚本或命令 |
| `cwd` | 否 | 命令的工作目录 |
| `timeout` | 否 | 超时时间，单位为毫秒，默认 300000（5 分钟） |
| `interpreter` | 否 | 脚本解释器，默认为 `cmd`；可设为 `cmd`、`pwsh` 或解释器的 Windows 绝对路径 |

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

### 下载文件

请求：

```http
POST /download
Content-Type: application/json
```

```json
{
  "path": "D:\\Desktop\\test.7z"
}
```

成功后响应体即为文件的二进制内容。

### 上传文件

请求体直接传输文件内容，通过 `X-File-Path` 请求头指定目标路径：

```powershell
curl.exe http://127.0.0.1:9527/upload `
  -H "Content-Type: application/octet-stream" `
  -H "X-File-Path: D:\Desktop\uploaded.7z" `
  --data-binary "@D:\Download\source.7z"
```

上传过程采用流式写入，不会将整个文件加载到内存。目标文件已存在时返回 `409 Conflict`，不会覆盖原文件。

成功响应示例：

```json
{
  "ok": true,
  "path": "D:\\Desktop\\uploaded.7z",
  "bytes": 123456
}
```

## 发布

推送新的 Git 标签后，GitHub Actions 会自动编译 64 位 Windows 版本、生成 `lcr-windows-x86_64.zip` 并创建对应的 GitHub Release。
