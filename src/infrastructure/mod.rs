// TermBridge infrastructure 层：SSH/SFTP/sshconfig/security 封装。
// Phase 0-C：sshconfig（ssh -G 解析）+ ssh（russh PTY 封装）。

pub mod ssh;
pub mod sshconfig;
