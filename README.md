# Lite Command RPC

简易的命令执行代理，适用于作为轻量级 OpenSSH Server 实现给 Agent 在目标环境中调试提供一个接入点

## 使用方法

### 通过 `cmd.exe` 执行命令
`POST /exec`

请求负载：
```json
{
  "command": "tasklist",
  "cwd": "D:\\Desktop"
}
```

响应示例：

```json
{
  "ok": true,
  "exit_code": 0,
  "stdout": "...",
  "stderr": "",
  "error": null
}
```

### 下载文件
`POST /download`

请求负载：
```json
{
    "path": "D:\\Desktop\\test.7z"
}
```