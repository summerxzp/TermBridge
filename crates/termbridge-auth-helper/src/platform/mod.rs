// 平台分发：按 target_os 选择 native 凭据输入实现（ADR-0019）。
//
// Unix（Linux / macOS）侧是多级 fallback resolver：
//   TERMBRIDGE_ASKPASS → 平台 GUI → TTY → Unsupported（可行动指引）
// Windows 侧保持 CredUI 原生对话框（ADR-0009），仅区分取消与环境失败。

#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod imp;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod imp;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod imp;

// Unix 共享：外部命令式 prompt（askpass / GUI dialog）+ POSIX tty prompt
#[cfg(unix)]
pub mod prompt_cmd;
#[cfg(unix)]
pub mod tty;

pub use imp::prompt_password;

/// `prompt_password` 的结果（协议 v2，ADR-0019）。
///
/// 区分四种终态，TermBridge 据此向 Agent 返回不同的错误语义：
/// - 用户取消 ≠ 环境不可用（旧协议把两者折叠成 cancelled，误导排障）
/// - 显式配置的 provider 损坏（Failed）不静默级联
#[derive(Debug)]
pub enum PromptOutcome {
    /// 用户提交了密码。
    ///
    /// `user`：对话框中实际编辑后的用户名。仅支持用户名编辑的 provider
    /// （Windows CredUI / zenity --username）回传 Some；其余回传 None，
    /// 调用方（TermBridge Core）回退请求中的预填用户名。
    Password {
        user: Option<String>,
        password: String,
    },
    /// 用户取消（对话框 / 终端已展示给用户后的取消动作）。
    Cancelled,
    /// 当前环境没有任何可用的输入通道，`message` 含可行动指引。
    Unsupported { message: String },
    /// provider 执行失败（如 TERMBRIDGE_ASKPASS 指向的程序损坏）。
    /// Windows CredUI 无此终态（环境失败归 Unsupported），仅 Unix askpass 使用。
    #[allow(dead_code)]
    Failed { message: String },
}
