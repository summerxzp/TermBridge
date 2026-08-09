//! 日志脱敏层（§5.5 三类正则）。
//!
//! 应用于 tracing 日志输出（stderr），防止 PTY 输出中的密码 / token / 私钥
//! 泄露到日志。三类正则：
//!   1. key=value / key: value 凭证（行尾脱敏）
//!   2. HTTP Authorization header
//!   3. PEM 私钥块（跨行）

use std::io::{self, Write};
use std::sync::OnceLock;

use regex::Regex;
use tracing_subscriber::fmt::MakeWriter;

// 三类脱敏正则，进程级编译一次复用（OnceLock 避免每次调用重新编译）。
static RE_CREDENTIAL: OnceLock<Regex> = OnceLock::new();
static RE_AUTHORIZATION: OnceLock<Regex> = OnceLock::new();
static RE_PEM_KEY: OnceLock<Regex> = OnceLock::new();

fn credential_re() -> &'static Regex {
    RE_CREDENTIAL.get_or_init(|| {
        // 1. key=value / key: value 凭证：匹配到行尾，整段替换为 [REDACTED]。
        //    捕获组 1 保留 key 前缀（含分隔符），便于回填。
        Regex::new(
            r"(?i)((?:password|passwd|secret|token|api[_-]?key|access[_-]?key|auth[_-]?token)\s*[=:]\s*)[^\n]+",
        )
        .expect("脱敏正则 RE_CREDENTIAL 编译失败")
    })
}

fn authorization_re() -> &'static Regex {
    RE_AUTHORIZATION.get_or_init(|| {
        // 2. HTTP Authorization header：Bearer/Basic/Token 后的凭证替换为 [REDACTED]。
        Regex::new(r"(?i)(Authorization:\s*(?:Bearer|Basic|Token)\s+)\S+")
            .expect("脱敏正则 RE_AUTHORIZATION 编译失败")
    })
}

fn pem_key_re() -> &'static Regex {
    RE_PEM_KEY.get_or_init(|| {
        // 3. PEM 私钥块（含 BEGIN/END 之间所有内容，跨行）。
        Regex::new(r"-----BEGIN [A-Z ]+PRIVATE KEY-----[\s\S]*?-----END [A-Z ]+PRIVATE KEY-----")
            .expect("脱敏正则 RE_PEM_KEY 编译失败")
    })
}

/// 对输入字符串应用三类脱敏正则，返回脱敏后的字符串。
///
/// 顺序：先 PEM 私钥块（跨行，避免被行级正则切碎），
/// 再凭证 key=value，最后 Authorization header。
pub fn redact(input: &str) -> String {
    let s = pem_key_re().replace_all(input, "[REDACTED PRIVATE KEY]");
    let s = credential_re().replace_all(&s, "${1}[REDACTED]");
    let s = authorization_re().replace_all(&s, "${1}[REDACTED]");
    s.into_owned()
}

/// 包装 stderr 的 writer，对写入的字节流按行缓冲并应用脱敏。
///
/// tracing fmt layer 输出是字节流，单条日志可能跨多次 `write` 调用。
/// 按行缓冲（以 `\n` 为界）确保每条日志完整后再脱敏，避免正则因行被截断而失效。
/// 残余未带换行符的尾部内容在 `flush` 时一并脱敏写出。
pub struct RedactingWriter {
    line_buf: Vec<u8>,
    sink: io::Stderr,
}

impl RedactingWriter {
    pub fn new() -> Self {
        Self {
            line_buf: Vec::new(),
            sink: io::stderr(),
        }
    }
}

impl Write for RedactingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // 累积到行缓冲
        self.line_buf.extend_from_slice(bytes);
        // 找到最后一个换行符：之前的完整行批量脱敏写出，之后的不完整行留在缓冲等下次
        if let Some(pos) = self.line_buf.iter().rposition(|&b| b == b'\n') {
            // split_off(pos+1) 返回 [pos+1, len)（剩余未完成行），self.line_buf 留下 [0, pos+1)（完整行）
            let remaining = self.line_buf.split_off(pos + 1);
            let to_write = std::mem::replace(&mut self.line_buf, remaining);
            let redacted = redact(&String::from_utf8_lossy(&to_write));
            self.sink.write_all(redacted.as_bytes())?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.line_buf.is_empty() {
            let redacted = redact(&String::from_utf8_lossy(&self.line_buf));
            self.sink.write_all(redacted.as_bytes())?;
            self.line_buf.clear();
        }
        self.sink.flush()
    }
}

impl Drop for RedactingWriter {
    fn drop(&mut self) {
        // 丢弃前刷出残余缓冲（若 tracing 未显式 flush，避免末尾日志丢失）
        let _ = self.flush();
    }
}

/// `MakeWriter` 实现：每次创建一个新的 `RedactingWriter`。
///
/// tracing-subscriber 对每条日志事件调用 `make_writer` 获取 writer，
/// 写完该事件后丢弃。单条日志的完整内容（含尾部 `\n`）会在该 writer 生命周期内
/// 到达，因此 per-instance 的行缓冲即可正确工作。
#[derive(Default)]
pub struct RedactingMakeWriter;

impl RedactingMakeWriter {
    pub fn new() -> Self {
        Self
    }
}

impl<'a> MakeWriter<'a> for RedactingMakeWriter {
    type Writer = RedactingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_password_assignment() {
        assert_eq!(redact("password=secret123"), "password=[REDACTED]");
    }

    #[test]
    fn redacts_authorization_bearer() {
        assert_eq!(
            redact("Authorization: Bearer abc.def.ghi"),
            "Authorization: Bearer [REDACTED]"
        );
    }

    #[test]
    fn redacts_pem_private_key() {
        let input = "-----BEGIN RSA PRIVATE KEY-----\n\
                     MIIEpAIBAAKCAQEA1234567890abcdef\n\
                     -----END RSA PRIVATE KEY-----";
        assert_eq!(redact(input), "[REDACTED PRIVATE KEY]");
    }

    #[test]
    fn keeps_normal_text_intact() {
        assert_eq!(redact("echo hello"), "echo hello");
        // 普通日志不应被误脱敏：host= 不在 key 名单内
        assert_eq!(
            redact("ssh connecting host=192.168.88.200"),
            "ssh connecting host=192.168.88.200"
        );
    }

    #[test]
    fn redacts_multiple_secrets_across_lines() {
        let input = "password=p1\ntoken=t1\nAuthorization: Bearer xyz789";
        let result = redact(input);
        assert_eq!(
            result,
            "password=[REDACTED]\ntoken=[REDACTED]\nAuthorization: Bearer [REDACTED]"
        );
    }

    #[test]
    fn redacts_all_secrets_in_single_line() {
        // [^\n]+ 贪婪到行尾：password= 之后整行被脱敏，p1 和 t1 均不残留
        let result = redact("password=p1 token=t1");
        assert!(!result.contains("p1"));
        assert!(!result.contains("t1"));
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn redact_is_case_insensitive() {
        assert_eq!(redact("Password: SecretValue"), "Password: [REDACTED]");
        assert_eq!(
            redact("AUTHORIZATION: BEARER mytoken"),
            "AUTHORIZATION: BEARER [REDACTED]"
        );
    }

    #[test]
    fn redacts_various_credential_keys() {
        assert_eq!(redact("api_key=abc"), "api_key=[REDACTED]");
        assert_eq!(redact("api-key=abc"), "api-key=[REDACTED]");
        assert_eq!(redact("access_key=abc"), "access_key=[REDACTED]");
        assert_eq!(redact("auth_token=abc"), "auth_token=[REDACTED]");
        assert_eq!(redact("secret: abc"), "secret: [REDACTED]");
    }
}
