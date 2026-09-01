# AGENTS.md

## 项目说明

这是一个仅面向 Windows 的轻量级 Rust HTTP 服务，二进制名称为 `lcr.exe`。

## 项目定位

- LCR 是面向受控 Windows 环境的轻量级远程操作入口，主要服务于 Agent、自动化工具和 Windows PE 排障场景。
- 保持单文件分发和较小的实现规模，不将项目扩展为通用远程管理平台。
- LCR 不提供身份认证，也不是安全沙箱。不要把 `work_dir`、命令白名单等请求策略描述为进程级安全隔离。
- 公网暴露、多租户、细粒度身份授权和强隔离不属于项目自身职责；相关能力应由外部认证、网络策略、低权限账户、ACL 或系统级沙箱提供。

## 开发约定

- 保持实现简单，优先使用标准库，谨慎增加依赖。
- 修改功能时需考虑更新 `README.md`、`.agents\skills\lcr\SKILL.md` 和 e2e 测试脚本。
- Windows 专属代码使用 `#[cfg(windows)]` 标记；不要破坏命令超时后终止进程树的行为。
- 提交前在 Windows 环境运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 和 `cargo test`。
- 不要提交 `target/` 目录或本地生成的二进制、压缩包。
