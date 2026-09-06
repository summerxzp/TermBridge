//! sshconfig —— `ssh -G <alias>` 解析为 domain::provider::Host（ADR-0006）
//!
//! 复用 OpenSSH 完整 config 解析能力（Include / Match / ProxyJump / Host *），
//! TermBridge 只消费 `ssh -G` 的最终输出，不自己实现 parser。
//!
//! ```text
//! resolve("testhost")
//!   → `ssh -G testhost` 子进程
//!   → 解析 `key value` 行
//!   → Host { name, hostname, user, port, identity_files, proxy_jump,
//!            user_known_hosts_files, strict_host_key_checking }
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::domain::provider::Host;
use crate::domain::provider::TermError;
use crate::infrastructure::username_store;

/// `ssh -G` 子进程超时（15s）。
///
/// 用户 ssh config 中 hung 的 `Match exec` 命令会让 `ssh -G` 永不退出 ——
/// 不加界则 resolve（进而 open_session / connect_session）永久挂起。
const SSH_G_TIMEOUT: Duration = Duration::from_secs(15);

/// 校验 `ssh -G` 的 alias 参数（P1-4 参数注入防护），返回 trim 后的 alias。
///
/// alias 直接拼为 `.arg("-G").arg(alias)` —— 以 `-` 开头的值（如 `-F/path`）
/// 会被 ssh 当作**选项**解析，可让 ssh 加载攻击者控制的 config 覆盖本次连接
/// 全部配置（hostname / IdentityFile / proxyjump 等）。拒绝：
/// - 空串 / 纯空白
/// - 以 `-` 开头（含恰为 `--`，后者是 ssh 的选项结束符，非合法别名）
///
/// 首尾空白决策（P1-4 review 待定项）：**先 trim 再校验** —— 首尾空白视为
/// 调用方噪音，trim 后的值同时用于 `ssh -G` 参数与 `Host.name`，避免空白
/// 混入 config 查找；含**内部**空格的值（如 `my host`）不构成选项注入，
/// 原样放行交给 ssh 报错。
fn validate_alias(alias: &str) -> Result<&str, TermError> {
    let trimmed = alias.trim();
    if trimmed.is_empty() {
        return Err(TermError::InvalidArgument(format!(
            "ssh alias 为空（alias={alias:?}）"
        )));
    }
    if trimmed.starts_with('-') {
        return Err(TermError::InvalidArgument(format!(
            "ssh alias 非法：以 '-' 开头会被 ssh 当作选项解析（疑似参数注入）: {alias:?}"
        )));
    }
    Ok(trimmed)
}

/// 调用 `ssh -G <alias>` 并解析为 Host。
///
/// `alias` 是 ssh config 里的 Host 别名（或直接 IP/hostname）。
///
/// 用户名优先级（高 → 低）：
/// 1. **记忆的用户名**（`username_store`）：用户在凭据对话框中填写过一次后
///    记住（最新优先），下次连接作为该 host 的默认用户名
/// 2. ssh config 的 `User`（`ssh -G` 输出；缺省时为空串，由后续认证路径处理）
///
/// 另：`identityfile` / `userknownhostsfile` 路径经 `normalize_msys_path`
/// 归一化——Git Bash (MSYS) 环境下 spawn 的 `ssh -G` 输出 POSIX 风格盘符路径
/// （`/c/Users/...`），Windows 侧 russh/std::fs 无法打开。
pub async fn resolve(alias: &str) -> Result<Host, TermError> {
    // 入口校验（P1-4）：拒绝空串 / '-' 开头 / `--`，防 ssh 选项注入
    let alias = validate_alias(alias)?;

    // ssh -G 走 stdio，快速返回，用 tokio::process 异步等。
    // stdin 置空：`ssh -G` 不读 stdin，但某些实现（如 Git for Windows 的
    // OpenSSH）会等 stdin EOF 才输出——MCP 场景 stdin 是长驻 transport，
    // 不置空会导致 open_session 永久挂起。
    //
    // P1-12：整体加 15s 超时 —— 用户 config 中 hung 的 `Match exec` 命令会让
    // `ssh -G` 永不退出，导致 resolve 永久挂起。
    let spawn = tokio::process::Command::new("ssh")
        .arg("-G")
        .arg(alias)
        .stdin(std::process::Stdio::null())
        .output();
    let output = match tokio::time::timeout(SSH_G_TIMEOUT, spawn).await {
        Ok(res) => res
            .map_err(|e| TermError::ConnectFailed(format!("spawn `ssh -G {alias}`: {e}")))?,
        Err(_) => {
            return Err(TermError::ConnectFailed(format!(
                "`ssh -G {alias}` timed out after {}s（用户 ssh config 中可能有挂起的 Match exec 命令）",
                SSH_G_TIMEOUT.as_secs()
            )));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TermError::ConnectFailed(format!(
            "`ssh -G {alias}` failed: {stderr}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut host = parse_ssh_g(alias, &stdout)?;

    // 记忆的用户名优先于 ssh config User（用户在凭据对话框填写过一次即成为
    // 该 host 的默认，最新优先；见 username_store 文档）
    if let Some(remembered) = username_store::lookup(&host.hostname) {
        tracing::debug!(
            host = %host.hostname,
            config_user = %host.user,
            remembered_user = %remembered,
            "sshconfig: 记忆的用户名覆盖 ssh config User"
        );
        host.user = remembered;
    }

    Ok(host)
}

/// 解析 `ssh -G` 输出文本为 Host（纯函数，便于单测）。
fn parse_ssh_g(alias: &str, stdout: &str) -> Result<Host, TermError> {
    let mut single: HashMap<String, String> = HashMap::new();
    let mut multi: HashMap<String, Vec<String>> = HashMap::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 格式：`key value`（空格分隔，key 全小写）
        let mut parts = line.splitn(2, char::is_whitespace);
        let key = parts.next().unwrap_or("").to_lowercase();
        let value = parts.next().unwrap_or("").trim().to_string();
        if key.is_empty() {
            continue;
        }
        match key.as_str() {
            "identityfile" | "certificatefile" => {
                multi.entry(key).or_default().push(value);
            }
            _ => {
                single.insert(key, value);
            }
        }
    }

    let hostname = single
        .get("hostname")
        .cloned()
        .ok_or_else(|| TermError::ConnectFailed(format!("`ssh -G {alias}` 缺少 hostname")))?;
    let user = single.get("user").cloned().unwrap_or_default();
    let port: u16 = single
        .get("port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(22);
    let proxy_jump = single
        .get("proxyjump")
        .filter(|v| !v.is_empty() && *v != "none")
        .cloned();

    // identityfile：收集所有存在的文件（展开 ~），保持 ssh -G 输出顺序。
    // Phase 1：支持多 IdentityFile 遍历（凭据优先级 SSH Agent > IdentityFile > HITL）。
    let identity_files: Vec<PathBuf> = multi
        .get("identityfile")
        .map(|files| {
            files
                .iter()
                .map(|s| expand_tilde(&normalize_msys_path(s, cfg!(windows))))
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();

    // userknownhostsfile：可能含多个空格分隔路径（OpenSSH 默认 `~/.ssh/known_hosts ~/.ssh/known_hosts2`）。
    // Phase 2：收集全部路径并展开 ~（之前 Phase 1 仅取首个）。
    // P3-6：按 OpenSSH token 语法切分 —— 双引号包裹的部分视为单个 token，
    // 使含空格的路径（"C:\Users\my name\known_hosts"、"/home/my user/kh"）
    // 不再被空白拆成多个 bogus 条目（与 identityfile 取整行 value 的处理同一精神）。
    // 不做 is_file 过滤——known_hosts 缺失本身是有意义状态（host 未知 / TOFU 首次写入），
    // 应让校验层报 "host 未知" 而非这里悄悄吞掉。空 Vec 表示 ssh -G 未输出该字段。
    let user_known_hosts_files: Vec<PathBuf> = single
        .get("userknownhostsfile")
        .map(|v| {
            split_quoted_tokens(v)
                .iter()
                .map(|s| expand_tilde(&normalize_msys_path(s, cfg!(windows))))
                .collect()
        })
        .unwrap_or_default();

    // stricthostkeychecking：ask / yes / no / accept-new（OpenSSH 默认 ask）。统一小写便于后续匹配。
    // accept-new：TOFU——首次连接（host 不在 known_hosts）自动添加 key（Phase 2）。
    let strict_host_key_checking = single
        .get("stricthostkeychecking")
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_else(|| "ask".to_string());

    Ok(Host {
        name: alias.to_string(),
        hostname,
        user,
        port,
        identity_files,
        proxy_jump,
        user_known_hosts_files,
        strict_host_key_checking,
    })
}

/// 展开 `~` 为用户 home 目录。非 `~` 开头原样返回。
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs_or_home() {
            return home.join(rest);
        }
    }
    if path == "~" {
        if let Some(home) = dirs_or_home() {
            return home;
        }
    }
    PathBuf::from(path)
}

/// 归一化 MSYS (Git Bash) 风格的 POSIX 路径为 Windows 可用形式。
///
/// 背景：MCP server 若从 Git Bash (MSYS) 环境被 spawn，PATH 上的 `ssh` 是
/// MSYS OpenSSH，`ssh -G` 输出 POSIX 风格盘符路径——
/// `userknownhostsfile /c/Users/.../.ssh/known_hosts`、
/// `identityfile /c/Users/.../.ssh/id_ed25519`。Windows 侧 russh / std::fs
/// 打不开 `/c/...` 路径 → host key 永远"未知"（HOST_KEY_REJECTED）、
/// identity file 静默失败。Windows 原生 OpenSSH 输出 `C:\Users\...` 无此问题。
///
/// 规则（`windows=true` 时）：路径匹配 `/<盘符>/`（如 `/c/`）→ 转为
/// `<盘符>:/...`（`/c/Users/foo` → `C:/Users/foo`；正斜杠对 Windows API 合法，
/// 不必转反斜杠）。其余（`/home/...`、`C:\...`、`~` 开头等）原样返回。
///
/// `windows=false`：恒原样返回（POSIX 上 `/c/...` 是合法字面路径）。
fn normalize_msys_path(path: &str, windows: bool) -> String {
    if !windows {
        return path.to_string();
    }
    // 仅处理 `/<单字母>/` 盘符形式；`/home/...`、`/tmp/...` 等不动
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b'/'
    {
        let mut out = String::with_capacity(path.len());
        // 保留原大小写（MSYS 通常输出小写盘符，Windows 不区分大小写均可打开）
        out.push(bytes[1] as char);
        out.push(':');
        out.push_str(&path[2..]); // 保留剩余 `/...`（正斜杠对 Windows API 合法）
        return out;
    }
    path.to_string()
}

/// 按 OpenSSH token 语法切分空格分隔的多 token 值（P3-6）。
///
/// 规则（与 OpenSSH config 的引号语义一致）：
/// - 双引号包裹的片段视为**单个 token**，其内部空格保留（含空格路径的关键）
/// - 引号可以与裸文本相邻（`"a b"c d` → `ab`、`d`），引号本身不出现在 token 中
/// - 未闭合引号：宽容处理，剩余内容并入当前 token（不丢路径；OpenSSH 严格模式
///   会报错，这里选择宽容 —— 误删用户 known_hosts 路径比接受残缺路径更危险）
///
/// 与 `identityfile` 的处理同一精神：identityfile 经 `splitn(2)` 取整行 value，
/// 天然保留空格；本函数把同样的能力带给一行多值的 `userknownhostsfile`。
fn split_quoted_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    // 当前 token 是否已有内容（引号本身也算内容：`" "` 是含一个空格的合法 token）
    let mut started = false;
    for c in value.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                started = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if started {
                    tokens.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        tokens.push(cur);
    }
    tokens
}

/// 取 home 目录（不引入 dirs crate，用 std + 环境变量）。
fn dirs_or_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

// ───────────────────────────────────────────────────────────────────────────
// ProxyJumpTarget —— ProxyJump 跳板机解析（§7.4 Phase 2）
// ───────────────────────────────────────────────────────────────────────────

/// ProxyJump 解析结果（§7.4 Phase 2）。
///
/// 解析 `ssh -G` 输出的 `proxyjump` 字段值，格式 `[user@]host[:port]`。
/// 缺省 user/port 时为 None —— 由调用方按 ssh config 默认值填充
/// （通过 `ssh -G <bastion>` 解析跳板机完整配置）。
///
/// **限制**（Phase 2 MVP）：
/// - 仅单跳：不支持逗号分隔的多跳链（`bastion1,bastion2`）。
/// - 不支持 IPv6 字面量（`[::1]:22`）—— 企业跳板机通常为域名/IPv4。
/// - 链式跳板由 `SshProvider::connect_session` 的递归 + MAX_PROXY_DEPTH 兜底。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyJumpTarget {
    /// 跳板机用户名（None 表示用 ssh config 默认值）。
    pub user: Option<String>,
    /// 跳板机主机名/IP。
    pub host: String,
    /// 跳板机端口（None 表示 22）。
    pub port: Option<u16>,
}

/// 解析 ProxyJump 字符串 `[user@]host[:port]` 为 [`ProxyJumpTarget`]。
///
/// 支持格式：
/// - `bastion` → host="bastion", user=None, port=None
/// - `user@bastion` → user=Some("user"), host="bastion", port=None
/// - `bastion:2222` → host="bastion", port=Some(2222), user=None
/// - `user@bastion:2222` → user=Some("user"), host="bastion", port=Some(2222)
///
/// 空字符串 / 多跳链 / host 为空 → `Err(InvalidArgument)`。
pub fn parse_proxy_jump(s: &str) -> Result<ProxyJumpTarget, TermError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(TermError::InvalidArgument(
            "proxyjump 为空字符串".into(),
        ));
    }

    // Phase 2 MVP：不支持逗号分隔的多跳链
    if s.contains(',') {
        return Err(TermError::InvalidArgument(format!(
            "proxyjump 多跳链不支持（Phase 2 仅单跳，深度上限见 MAX_PROXY_DEPTH）: {s}"
        )));
    }

    // 拆分 user@...（取首个 '@'；空用户名视为 None）
    let (user, rest) = match s.find('@') {
        Some(pos) => {
            let u = s[..pos].to_string();
            let r = &s[pos + 1..];
            (if u.is_empty() { None } else { Some(u) }, r)
        }
        None => (None, s),
    };

    // 拆分 host:port（找最后一个 ':'，且其后为有效 u16 才算 port）
    // 不支持 IPv6 字面量（含多个 ':' 或 '[...]'）—— MVP 限制
    let (host, port) = match rest.rfind(':') {
        Some(pos) => {
            let maybe_port = &rest[pos + 1..];
            match maybe_port.parse::<u16>() {
                Ok(port) => (rest[..pos].to_string(), Some(port)),
                // ':' 后非数字 → 视为 host 的一部分（如 IPv6 无括号）
                Err(_) => (rest.to_string(), None),
            }
        }
        None => (rest.to_string(), None),
    };

    if host.is_empty() {
        return Err(TermError::InvalidArgument(format!(
            "proxyjump host 为空: {s}"
        )));
    }

    Ok(ProxyJumpTarget { user, host, port })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 序列化所有操作 HOME 环境变量的测试，避免并行竞争。
    /// Windows 上 HOME 不控制 home 目录，但 expand_tilde 读它，多测试并行 set/restore 会竞态。
    static HOME_ENV_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    const SAMPLE_G: &str = r#"user testuser
hostname 203.0.113.140
port 22
identityfile ~/.ssh/id_ed25519
identityfile ~/.ssh/id_rsa
userknownhostsfile ~/.ssh/known_hosts
stricthostkeychecking ask
identitiesonly yes
proxyjump none
"#;

    #[test]
    fn parse_basic_fields() {
        let host = parse_ssh_g("testhost", SAMPLE_G).unwrap();
        assert_eq!(host.name, "testhost");
        assert_eq!(host.hostname, "203.0.113.140");
        assert_eq!(host.user, "testuser");
        assert_eq!(host.port, 22);
        assert_eq!(host.proxy_jump, None);
    }

    #[test]
    fn parse_multi_identityfile_collects_all_existing() {
        // 多个 identityfile：收集所有存在的文件，保持顺序；跳过不存在的
        let temp_dir = std::env::temp_dir();
        let temp_key_a = temp_dir.join("termbridge_test_id_ed25519_a");
        let temp_key_b = temp_dir.join("termbridge_test_id_rsa_b");
        std::fs::write(&temp_key_a, "dummy_key_content_a").unwrap();
        std::fs::write(&temp_key_b, "dummy_key_content_b").unwrap();

        let g = format!(
            "user testuser\nhostname 203.0.113.140\nport 22\nidentityfile {}\nidentityfile {}\nidentityfile /nonexistent_key\n",
            temp_key_a.display(),
            temp_key_b.display()
        );
        let host = parse_ssh_g("testhost", &g).unwrap();
        assert_eq!(
            host.identity_files.len(),
            2,
            "应收集两个存在的 key 文件（跳过不存在的）"
        );
        assert_eq!(host.identity_files[0], temp_key_a);
        assert_eq!(host.identity_files[1], temp_key_b);

        // cleanup
        std::fs::remove_file(&temp_key_a).ok();
        std::fs::remove_file(&temp_key_b).ok();
    }

    #[test]
    fn parse_identityfile_empty_when_none_exist() {
        // 全部 identityfile 都不存在 → 空 Vec
        let g = "user testuser\nhostname 203.0.113.140\nport 22\nidentityfile /nonexistent_a\nidentityfile /nonexistent_b\n";
        let host = parse_ssh_g("testhost", g).unwrap();
        assert!(host.identity_files.is_empty());
    }

    #[test]
    fn parse_identityfile_empty_when_absent() {
        // 无 identityfile 行 → 空 Vec
        let g = "user u\nhostname h\nport 22\n";
        let host = parse_ssh_g("testhost", g).unwrap();
        assert!(host.identity_files.is_empty());
    }

    #[test]
    fn parse_proxyjump_when_set() {
        let g = r#"user u
hostname h
port 22
proxyjump bastion
"#;
        let host = parse_ssh_g("via", g).unwrap();
        assert_eq!(host.proxy_jump.as_deref(), Some("bastion"));
    }

    // ── Phase 2：ProxyJumpTarget 解析单元测试 ────────────────────────────

    #[test]
    fn parse_proxy_jump_bare_host() {
        // 纯主机名：user=None, port=None
        let t = parse_proxy_jump("bastion").unwrap();
        assert_eq!(t.user, None);
        assert_eq!(t.host, "bastion");
        assert_eq!(t.port, None);
    }

    #[test]
    fn parse_proxy_jump_user_at_host() {
        // user@bastion：user=Some, port=None
        let t = parse_proxy_jump("ops@bastion").unwrap();
        assert_eq!(t.user.as_deref(), Some("ops"));
        assert_eq!(t.host, "bastion");
        assert_eq!(t.port, None);
    }

    #[test]
    fn parse_proxy_jump_host_port() {
        // bastion:2222：user=None, port=Some(2222)
        let t = parse_proxy_jump("bastion:2222").unwrap();
        assert_eq!(t.user, None);
        assert_eq!(t.host, "bastion");
        assert_eq!(t.port, Some(2222));
    }

    #[test]
    fn parse_proxy_jump_user_host_port() {
        // user@bastion:2222：完整格式
        let t = parse_proxy_jump("ops@bastion:2222").unwrap();
        assert_eq!(t.user.as_deref(), Some("ops"));
        assert_eq!(t.host, "bastion");
        assert_eq!(t.port, Some(2222));
    }

    #[test]
    fn parse_proxy_jump_empty_user() {
        // @bastion：空用户名视为 None（不报错）
        let t = parse_proxy_jump("@bastion").unwrap();
        assert_eq!(t.user, None);
        assert_eq!(t.host, "bastion");
    }

    #[test]
    fn parse_proxy_jump_trims_whitespace() {
        // 前后空格应被 trim
        let t = parse_proxy_jump("  bastion  ").unwrap();
        assert_eq!(t.host, "bastion");
    }

    #[test]
    fn parse_proxy_jump_ipv4_address() {
        // IPv4 地址作为 host
        let t = parse_proxy_jump("10.0.0.1:2222").unwrap();
        assert_eq!(t.host, "10.0.0.1");
        assert_eq!(t.port, Some(2222));
    }

    #[test]
    fn parse_proxy_jump_rejects_empty_string() {
        let err = parse_proxy_jump("").unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_proxy_jump_rejects_whitespace_only() {
        let err = parse_proxy_jump("   ").unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_proxy_jump_rejects_multi_hop() {
        // Phase 2 MVP 不支持多跳链
        let err = parse_proxy_jump("bastion1,bastion2").unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
        assert!(format!("{err}").contains("多跳链"));
    }

    #[test]
    fn parse_proxy_jump_rejects_empty_host_after_at() {
        // user@ → host 为空
        let err = parse_proxy_jump("ops@").unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_proxy_jump_rejects_empty_host_before_port() {
        // :2222 → host 为空
        let err = parse_proxy_jump(":2222").unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_proxy_jump_non_numeric_port_treated_as_host() {
        // ':' 后非数字 → 整个当作 host（不报错，兼容 IPv6 无括号）
        let t = parse_proxy_jump("bastion:notaport").unwrap();
        assert_eq!(t.host, "bastion:notaport");
        assert_eq!(t.port, None);
    }

    #[test]
    fn parse_proxy_jump_port_overflow_treated_as_host() {
        // 端口超出 u16 范围 → parse 失败 → 当作 host
        let t = parse_proxy_jump("bastion:99999").unwrap();
        assert_eq!(t.host, "bastion:99999");
        assert_eq!(t.port, None);
    }

    #[test]
    fn parse_missing_hostname_errors() {
        let g = r#"user u
port 22
"#;
        let err = parse_ssh_g("bad", g).unwrap_err();
        assert_eq!(err.code(), "CONNECT_FAILED");
    }

    #[test]
    fn expand_tilde_home() {
        let _guard = HOME_ENV_LOCK.lock();
        // 保存并恢复 HOME，避免影响并行测试
        let saved_home = std::env::var_os("HOME");
        std::env::set_var("HOME", "/tmp/fakehome");

        let p = expand_tilde("~/foo/bar");
        // Windows 上 join 用 \ 分隔，比较 PathBuf 而非字符串
        let expected = PathBuf::from("/tmp/fakehome").join("foo").join("bar");
        assert_eq!(p, expected);
        assert!(!p.to_string_lossy().contains('~'));

        let p2 = expand_tilde("/abs/path");
        assert_eq!(p2, PathBuf::from("/abs/path"));

        // 恢复
        match saved_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn parse_custom_port() {
        let g = r#"user u
hostname h
port 2222
"#;
        let host = parse_ssh_g("custom", g).unwrap();
        assert_eq!(host.port, 2222);
    }

    #[test]
    fn parse_user_known_hosts_files_expands_tilde() {
        let _guard = HOME_ENV_LOCK.lock();
        // 保存并恢复 HOME，避免影响并行测试
        let saved_home = std::env::var_os("HOME");
        std::env::set_var("HOME", "/tmp/fakehome");

        let host = parse_ssh_g("testhost", SAMPLE_G).unwrap();
        // SAMPLE_G 只有一个 userknownhostsfile 路径
        assert_eq!(host.user_known_hosts_files.len(), 1, "单路径应收集为 1 元素 Vec");
        let p = &host.user_known_hosts_files[0];
        let expected = PathBuf::from("/tmp/fakehome").join(".ssh").join("known_hosts");
        assert_eq!(p, &expected);
        assert!(!p.to_string_lossy().contains('~'));

        match saved_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn parse_user_known_hosts_files_collects_all_when_multiple() {
        let _guard = HOME_ENV_LOCK.lock();
        // OpenSSH 默认 `~/.ssh/known_hosts ~/.ssh/known_hosts2`，
        // Phase 2 应收集全部（Phase 1 仅取首个）。
        let saved_home = std::env::var_os("HOME");
        std::env::set_var("HOME", "/tmp/fakehome");

        let g = r#"user u
hostname h
port 22
userknownhostsfile ~/.ssh/known_hosts ~/.ssh/known_hosts2
"#;
        let host = parse_ssh_g("multi", g).unwrap();
        assert_eq!(
            host.user_known_hosts_files.len(),
            2,
            "应收集全部 2 个路径（Phase 2 多文件支持）"
        );
        let expected1 = PathBuf::from("/tmp/fakehome").join(".ssh").join("known_hosts");
        let expected2 = PathBuf::from("/tmp/fakehome").join(".ssh").join("known_hosts2");
        assert_eq!(host.user_known_hosts_files[0], expected1);
        assert_eq!(host.user_known_hosts_files[1], expected2);

        match saved_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn parse_strict_host_key_checking_values() {
        // SAMPLE_G 用 "ask"
        let host = parse_ssh_g("testhost", SAMPLE_G).unwrap();
        assert_eq!(host.strict_host_key_checking, "ask");

        // yes
        let g = r#"user u
hostname h
port 22
stricthostkeychecking yes
"#;
        let host = parse_ssh_g("strict_yes", g).unwrap();
        assert_eq!(host.strict_host_key_checking, "yes");

        // no
        let g = r#"user u
hostname h
port 22
stricthostkeychecking no
"#;
        let host = parse_ssh_g("strict_no", g).unwrap();
        assert_eq!(host.strict_host_key_checking, "no");

        // accept-new（Phase 2 TOFU）
        let g = r#"user u
hostname h
port 22
stricthostkeychecking accept-new
"#;
        let host = parse_ssh_g("strict_accept_new", g).unwrap();
        assert_eq!(host.strict_host_key_checking, "accept-new");

        // 大写应归一化为小写
        let g = r#"user u
hostname h
port 22
stricthostkeychecking YES
"#;
        let host = parse_ssh_g("strict_upper", g).unwrap();
        assert_eq!(host.strict_host_key_checking, "yes");
    }

    #[test]
    fn parse_strict_host_key_checking_defaults_to_ask() {
        // 缺省 stricthostkeychecking → "ask"（OpenSSH 默认）
        let g = r#"user u
hostname h
port 22
"#;
        let host = parse_ssh_g("default", g).unwrap();
        assert_eq!(host.strict_host_key_checking, "ask");
    }

    #[test]
    fn parse_user_known_hosts_files_empty_when_absent() {
        // 缺省 userknownhostsfile → 空 Vec
        let g = r#"user u
hostname h
port 22
"#;
        let host = parse_ssh_g("default", g).unwrap();
        assert!(
            host.user_known_hosts_files.is_empty(),
            "缺省 userknownhostsfile 应为空 Vec"
        );
    }

    // ── P1-4：alias 入口校验（防 `ssh -G` 参数注入） ────────────────────

    #[test]
    fn validate_alias_rejects_dash_prefixed_injection() {
        // 以 '-' 开头：ssh 会把 `-F /tmp/evil` 当选项解析（可加载攻击者控制的 config）
        let err = validate_alias("-F /tmp/evil").unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
        assert!(format!("{err}").contains("参数注入"));
    }

    #[test]
    fn validate_alias_rejects_double_dash() {
        // `--` 是 ssh 的选项结束符，作为别名同样拒绝
        let err = validate_alias("--").unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
    }

    #[test]
    fn validate_alias_rejects_single_dash_and_dash_only_prefixes() {
        let err = validate_alias("-").unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
        let err2 = validate_alias("-oProxyCommand=evil").unwrap_err();
        assert_eq!(err2.code(), "INVALID_ARGUMENT");
    }

    #[test]
    fn validate_alias_rejects_empty() {
        let err = validate_alias("").unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
        // 纯空白 trim 后为空 → 同样拒绝
        let err2 = validate_alias("   ").unwrap_err();
        assert_eq!(err2.code(), "INVALID_ARGUMENT");
    }

    #[test]
    fn validate_alias_trims_surrounding_whitespace() {
        // 决策：先 trim 再校验 —— 首尾空白视为调用方噪音，trim 后放行，
        // 且返回值（trim 后）同时用于 ssh -G 参数与 Host.name
        assert_eq!(validate_alias(" legitimate ").unwrap(), "legitimate");
    }

    #[test]
    fn validate_alias_accepts_normal_aliases() {
        assert_eq!(validate_alias("testhost").unwrap(), "testhost");
        assert_eq!(validate_alias("203.0.113.140").unwrap(), "203.0.113.140");
        // 内部空格不构成选项注入，原样放行（交给 ssh 报 unknown host）
        assert_eq!(validate_alias("my host").unwrap(), "my host");
    }

    // ── P3-6：userknownhostsfile 引号 token（含空格路径） ───────────────

    #[test]
    fn split_quoted_tokens_basic() {
        // 无引号：退化为普通空白切分
        assert_eq!(split_quoted_tokens("a b c"), vec!["a", "b", "c"]);
        // 引号包裹：内部空格保留为单 token
        assert_eq!(split_quoted_tokens("\"a b\" c"), vec!["a b", "c"]);
        // 连续引号段与裸文本相邻：引号本身不出现在 token 中（`"a b"c` → `a bc`）
        assert_eq!(split_quoted_tokens("\"a b\"c d"), vec!["a bc", "d"]);
        // 未闭合引号：剩余内容并入当前 token（宽容处理）
        assert_eq!(split_quoted_tokens("\"a b"), vec!["a b"]);
        // 空串 / 纯空白 → 空 Vec
        assert!(split_quoted_tokens("").is_empty());
        assert!(split_quoted_tokens("   ").is_empty());
    }

    #[test]
    fn parse_user_known_hosts_file_quoted_path_with_spaces() {
        // POSIX 风格含空格路径：引号包裹 → 单 token，空格保留
        let g = "user u\nhostname h\nport 22\nuserknownhostsfile \"/home/my user/kh\"\n";
        let host = parse_ssh_g("quoted", g).unwrap();
        assert_eq!(
            host.user_known_hosts_files.len(),
            1,
            "引号包裹的含空格路径应为单个 token"
        );
        assert_eq!(
            host.user_known_hosts_files[0],
            PathBuf::from("/home/my user/kh"),
            "引号内的空格不应被拆分"
        );
    }

    #[test]
    fn parse_user_known_hosts_files_mixed_quoted_and_plain() {
        // Windows 风格含空格引号路径 + 普通无引号路径混排
        let g = "user u\nhostname h\nport 22\nuserknownhostsfile \"C:\\Users\\my name\\kh\" /tmp/kh2\n";
        let host = parse_ssh_g("mixed", g).unwrap();
        assert_eq!(host.user_known_hosts_files.len(), 2);
        assert_eq!(
            host.user_known_hosts_files[0],
            PathBuf::from("C:\\Users\\my name\\kh")
        );
        assert_eq!(host.user_known_hosts_files[1], PathBuf::from("/tmp/kh2"));
    }

    #[test]
    fn parse_user_known_hosts_files_unquoted_multi_path_still_works() {
        // 回归保护：无引号多路径行为与之前一致（不依赖 HOME，路径不含 ~）
        let g = "user u\nhostname h\nport 22\nuserknownhostsfile /tmp/kh1 /tmp/kh2\n";
        let host = parse_ssh_g("plain", g).unwrap();
        assert_eq!(host.user_known_hosts_files.len(), 2);
        assert_eq!(host.user_known_hosts_files[0], PathBuf::from("/tmp/kh1"));
        assert_eq!(host.user_known_hosts_files[1], PathBuf::from("/tmp/kh2"));
    }

    // ── MSYS (Git Bash) 路径归一化 ─────────────────────────────────────
    // 背景：Git Bash 环境 spawn 的 `ssh -G` 输出 POSIX 风格盘符路径
    // （`/c/Users/...`），Windows 侧 russh/std::fs 打不开。

    #[test]
    fn normalize_msys_path_converts_drive_letter_form_on_windows() {
        // `/c/Users/SUMMER/.ssh/known_hosts` → `c:/Users/SUMMER/.ssh/known_hosts`
        // （保留原盘符大小写；MSYS 输出小写，Windows 文件系统不区分大小写）
        assert_eq!(
            normalize_msys_path("/c/Users/SUMMER/.ssh/known_hosts", true),
            "c:/Users/SUMMER/.ssh/known_hosts"
        );
        // 大写盘符同样处理
        assert_eq!(
            normalize_msys_path("/D/data/id_ed25519", true),
            "D:/data/id_ed25519"
        );
        // 恰为 `/<盘符>/` 前缀的最短形式（`/c/`）也应转换
        assert_eq!(normalize_msys_path("/c/", true), "c:/");
    }

    #[test]
    fn normalize_msys_path_leaves_posix_paths_unchanged_on_windows() {
        // 非 `/<单字母>/` 形式的 POSIX 路径不动（可能是合法远端/字面路径）
        assert_eq!(
            normalize_msys_path("/home/user/x", true),
            "/home/user/x"
        );
        assert_eq!(normalize_msys_path("/tmp/kh", true), "/tmp/kh");
        // `/<双字符>/` 不是盘符形式
        assert_eq!(normalize_msys_path("/ab/cd", true), "/ab/cd");
    }

    #[test]
    fn normalize_msys_path_leaves_windows_paths_unchanged() {
        // 已是 Windows 形式（反斜杠 / 盘符冒号）原样返回
        assert_eq!(
            normalize_msys_path(r"C:\already\fine", true),
            r"C:\already\fine"
        );
        assert_eq!(normalize_msys_path("C:/Users/x", true), "C:/Users/x");
        // 相对路径不动
        assert_eq!(normalize_msys_path("relative/path", true), "relative/path");
    }

    #[test]
    fn normalize_msys_path_noop_on_non_windows() {
        // POSIX 上 `/c/...` 是合法字面路径，恒原样返回
        assert_eq!(
            normalize_msys_path("/c/Users/SUMMER/.ssh/known_hosts", false),
            "/c/Users/SUMMER/.ssh/known_hosts"
        );
        assert_eq!(normalize_msys_path("/home/user/x", false), "/home/user/x");
    }

    /// 记忆用户名覆盖辅助：从解析结果 Host 应用记忆（resolve 内联逻辑提取，
    /// 纯函数便于单测——resolve 本身依赖 `ssh -G` 子进程无法单测，
    /// `username_store::lookup` 读全局文件也不适合在单测中预置）。
    fn apply_remembered_user(host: &mut Host, remembered: Option<String>) {
        if let Some(user) = remembered {
            host.user = user;
        }
    }

    #[test]
    fn remembered_user_overrides_config_user() {
        // 有记忆 → 覆盖 ssh config User（"填写过一次后作为默认"）
        let mut host = parse_ssh_g("testhost", SAMPLE_G).unwrap();
        assert_eq!(host.user, "testuser");
        apply_remembered_user(&mut host, Some("alice".into()));
        assert_eq!(host.user, "alice");
    }

    #[test]
    fn no_remembered_user_keeps_config_user() {
        // 无记忆 → 保持 ssh config User（首次使用，用户名默认空/取 config）
        let mut host = parse_ssh_g("testhost", SAMPLE_G).unwrap();
        apply_remembered_user(&mut host, None);
        assert_eq!(host.user, "testuser");
    }
}
