// Windows native 凭据对话框：通过 CredUIPromptForCredentialsW (credui.dll)
// 弹出系统原生凭据输入框，支持密码掩码。用户取消或任何错误统一回 Cancelled。
//
// 安全：密码缓冲读取后立即用 write_volatile 清零（模拟 SecureZeroMemory），
// 避免依赖额外 windows feature。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{FALSE, ERROR_SUCCESS};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::Security::Credentials::{
    CredUIPromptForCredentialsW, CREDUI_FLAGS_ALWAYS_SHOW_UI, CREDUI_FLAGS_DO_NOT_PERSIST,
    CREDUI_FLAGS_GENERIC_CREDENTIALS, CREDUI_INFOW,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

pub enum PromptError {
    Cancelled,
    #[allow(dead_code)]
    Unsupported,
}

pub fn prompt_password(host: &str, user: &str, reason: &str) -> Result<String, PromptError> {
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
                let password = from_wide_buf(&password_buf);
                secure_zero(&mut user_buf);
                secure_zero(&mut password_buf);
                Ok(password)
            }
            // ERROR_CANCELLED 或任何其它失败：保守当取消，不暴露内部错误
            _ => {
                secure_zero(&mut user_buf);
                secure_zero(&mut password_buf);
                Err(PromptError::Cancelled)
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
