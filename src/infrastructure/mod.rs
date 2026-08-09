// TermBridge infrastructure 层：SSH/SFTP/sshconfig/security 封装。
// Phase 0-C：sshconfig（ssh -G 解析）+ ssh（russh PTY 封装）。
// Phase 1：redact（日志脱敏 §5.5）+ sftp（russh-sftp 封装 §7.4）。

pub mod redact;
pub mod sftp;
pub mod ssh;
pub mod sshconfig;
