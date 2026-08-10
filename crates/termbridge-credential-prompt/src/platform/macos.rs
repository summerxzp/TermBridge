// macOS stub：ADR-0009 阶段 B1 仅要求 Windows 真实实现。
// 真实实现（Security framework / Authorization 相关注入）留待后续阶段。

pub enum PromptError {
    Cancelled,
    Unsupported,
}

pub fn prompt_password(_host: &str, _user: &str, _reason: &str) -> Result<String, PromptError> {
    Err(PromptError::Unsupported)
}
