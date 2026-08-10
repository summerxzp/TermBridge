// TermBridge infrastructure 层：SSH/SFTP/sshconfig/security 封装。
// Phase 0-C：sshconfig（ssh -G 解析）+ ssh（russh PTY 封装）。
// Phase 1：redact（日志脱敏 §5.5）+ sftp（russh-sftp 封装 §7.4）。
// Phase 3-A：daemon_proto（远端 daemon 协议编解码 ADR-0004 §4）+ persistent（daemon RPC client + PersistentProvider）。

pub mod credential;
pub mod daemon_proto;
pub mod persistent;
pub mod redact;
pub mod sftp;
pub mod ssh;
pub mod sshconfig;
