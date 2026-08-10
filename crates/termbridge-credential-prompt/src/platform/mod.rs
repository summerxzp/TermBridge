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

#[allow(unused_imports)]
pub use imp::{prompt_password, PromptError};
