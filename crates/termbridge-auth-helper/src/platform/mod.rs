// 平台分发：按 target_os 选择对应的 native 凭据对话框实现。
// macOS / Linux 当前为 stub（返回 Unsupported），仅 Windows 真实实现。

#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod imp;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod imp;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod imp;

/// `prompt_password` 的返回：密码 + 对话框中实际确认/编辑的用户名。
///
/// Windows CredUI 的用户名缓冲是 in/out 的——用户可以编辑预填的用户名
/// （如把 ssh config 的 root 改成普通用户），必须读回传给 TermBridge。
/// POSIX tty prompt 不提供用户名编辑，原样返回请求的预填值。
pub struct PromptedCredential {
    /// 对话框中实际确认/编辑的用户名
    pub user: String,
    /// 密码
    pub password: String,
}

#[allow(unused_imports)]
pub use imp::{prompt_password, PromptError};
