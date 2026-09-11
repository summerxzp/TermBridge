// Windows native 凭据对话框：通过 CredUIPromptForCredentialsW (credui.dll)
// 弹出系统原生凭据输入框，支持密码掩码。
//
// 协议 v2（ADR-0019）：用户取消回 Cancelled；CredUI 环境失败（无交互
// 桌面会话，如 SSH 远程会话 / 服务进程启动的 MCP server）回
// Unsupported——两者不再折叠，Agent 才能给出正确的排障指引。
//
// 安全：密码缓冲读取后立即用 write_volatile 清零（模拟 SecureZeroMemory），
// 避免依赖额外 windows feature。

use super::PromptOutcome;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_SUCCESS, FALSE};
// ERROR_CANCELLED 是常量不是类型，经命名空间别名引入避免 non_snake_case 告警
use windows::Win32::Foundation as win;
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::Security::Credentials::{
    CredUIPromptForCredentialsW, CREDUI_FLAGS_ALWAYS_SHOW_UI, CREDUI_FLAGS_DO_NOT_PERSIST,
    CREDUI_FLAGS_GENERIC_CREDENTIALS, CREDUI_INFOW,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

pub fn prompt_password(host: &str, user: &str, reason: &str) -> PromptOutcome {
    unsafe {
        // CREDUI 用户名/密码缓冲（WCHAR 计数）。CredUI wrapper 用切片 len 作 max chars。
        const USER_BUF_LEN: usize = 256;
        const PASSWORD_BUF_LEN: usize = 512;

        let mut user_buf = [0u16; USER_BUF_LEN];
        // 预填用户名（pszUserName 是 in/out 缓冲），对话框显示当前 user
        let user_wide = to_wide(user);
        let copy = user_wide.len().min(user_buf.len());
        user_buf[..copy].copy_from_slice(&user_wide[..copy]);

        let mut password_buf = [0u16; PASSWORD_BUF_LEN];

        // "保存" 复选框（DO_NOT_PERSIST 下不实际保存，仅占位）
        let mut save = FALSE;

        let caption = to_wide("TermBridge Credential");
        let message_text = to_wide(&format!(
            "Host: {}\nUser: {}\nReason: {}",
            host, user, reason
        ));
        let target = to_wide(&format!("TermBridge:{}", host));

        // 用当前前台窗口作为父窗口，避免 CredUI 对话框在 NULL 父窗口下不显示
        let hwnd_parent = GetForegroundWindow();

        let info = CREDUI_INFOW {
            cbSize: std::mem::size_of::<CREDUI_INFOW>() as u32,
            hwndParent: hwnd_parent,
            pszMessageText: PCWSTR(message_text.as_ptr()),
            pszCaptionText: PCWSTR(caption.as_ptr()),
            hbmBanner: HBITMAP::default(),
        };

        let flags = CREDUI_FLAGS_GENERIC_CREDENTIALS
            | CREDUI_FLAGS_DO_NOT_PERSIST
            | CREDUI_FLAGS_ALWAYS_SHOW_UI;

        let result = CredUIPromptForCredentialsW(
            Some(std::ptr::addr_of!(info)),
            PCWSTR(target.as_ptr()),
            None,
            0,
            &mut user_buf,
            &mut password_buf,
            Some(std::ptr::addr_of_mut!(save)),
            flags,
        );

        match result {
            ERROR_SUCCESS => {
                // user_buf 是 in/out 缓冲：对话框允许用户编辑用户名，
                // 必须在清零前读回（可能是用户改过的自定义登录名）
                let user_out = from_wide_buf(&user_buf);
                let password = from_wide_buf(&password_buf);
                secure_zero(&mut user_buf);
                secure_zero(&mut password_buf);
                PromptOutcome::Password {
                    user: Some(user_out),
                    password,
                }
            }
            win::ERROR_CANCELLED => {
                secure_zero(&mut user_buf);
                secure_zero(&mut password_buf);
                PromptOutcome::Cancelled
            }
            // 其它失败码（ERROR_NO_SUCH_LOGON_SESSION 等）：CredUI 无法在
            // 当前会话展示对话框（典型：无交互桌面），按环境不可用上报
            _ => {
                secure_zero(&mut user_buf);
                secure_zero(&mut password_buf);
                PromptOutcome::Unsupported {
                    message: format!(
                        "CredUI prompt failed with Windows error {} \
(no interactive desktop session?). Options: (1) run TermBridge from an \
interactive desktop session; (2) set TERMBRIDGE_ASKPASS to an \
askpass-compatible program.",
                        result.0
                    ),
                }
            }
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide_buf(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn secure_zero(buf: &mut [u16]) {
    for b in buf.iter_mut() {
        unsafe { std::ptr::write_volatile(b as *mut u16, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}
