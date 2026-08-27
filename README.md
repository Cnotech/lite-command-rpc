# Lite Command RPC

简易的命令执行代理，通过 `POST /exec` 发送请求即可在代理内通过 `cmd.exe` 执行命令，适用于给 Agent 在目标环境中调试提供一个接入点

## 使用方法

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