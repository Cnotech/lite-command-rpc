# AGENTS.md

## 项目说明

这是一个仅面向 Windows 的轻量级 Rust HTTP 服务，二进制名称为 `lcr.exe`。

## 开发约定

- 保持实现简单，优先使用标准库，谨慎增加依赖。
- 修改接口时同步更新 `README.md`。
- Windows 专属代码使用 `#[cfg(windows)]` 标记；不要破坏命令超时后终止进程树的行为。
- 提交前在 Windows 环境运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 和 `cargo test`。
- 不要提交 `target/` 目录或本地生成的二进制、压缩包。
