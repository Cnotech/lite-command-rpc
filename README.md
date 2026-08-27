# Lite Command RPC

简易的命令执行代理，适用于作为轻量级 OpenSSH Server 实现给 Agent 在目标环境中调试提供一个接入点

## 使用方法

### 通过 `cmd.exe` 执行命令
`POST /exec`

请求负载：
```json
{
  "command": "tasklist",
  "cwd": "D:\\Desktop",
  "timeout": 300000
}
```

`timeout` 单位为毫秒，可选，默认值为 `300000`（5 分钟）。超时后服务会终止命令进程树，并返回 `timed_out: true`。
命令会话使用 UTF-8 代码页，因此中文 stdout/stderr 可以直接通过 JSON 返回。

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

`POST /exec/stream`

请求负载与 `/exec` 相同。响应使用 HTTP chunked 传输，内容类型为 NDJSON；命令执行期间会持续返回事件：

```json
{"type":"stdout","data":"hello\r\n"}
{"type":"stderr","data":"warning\r\n"}
{"type":"exit","exit_code":0}
```

超时时最后一条事件为：

```json
{"type":"timeout","timeout":300000}
```

如果命令无法启动，则返回 `error` 事件。

### 下载文件
`POST /download`

请求负载：
```json
{
    "path": "D:\\Desktop\\test.7z"
}
```

### 上传文件

`POST /upload`

请求体直接传输文件内容，并通过 `X-File-Path` 请求头指定目标路径。上传过程为流式写入，不会把整个文件加载到内存；目标文件已存在时返回 `409 Conflict`，不会覆盖。

```powershell
curl.exe http://127.0.0.1:9527/upload `
  -H "Content-Type: application/octet-stream" `
  -H "X-File-Path: D:\Desktop\uploaded.7z" `
  --data-binary "@D:\Download\source.7z"
```

成功时返回：

```json
{
  "ok": true,
  "path": "D:\\Desktop\\uploaded.7z",
  "bytes": 123456
}
```
