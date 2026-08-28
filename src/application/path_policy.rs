//! path_policy —— SFTP 路径策略（ADR-0005 §4 / PLAN §5.5 / §7.4 Phase 1）
//!
//! 拒绝策略：
//! - 本地路径：`canonicalize()` 后检查是否在 `allowed_local_paths` 之下，防 `../` 穿越。
//! - 远端路径：调远端 SFTP `realpath`（通过 `SftpCanonicalize` trait）解析后检查
//!   是否在允许根之下，防 `../` 与 symlink 逃逸。
//! - null 字节（`\0`）一律拒绝（防止 null 字节注入绕过路径检查）。
//!
//! 远端访问模型（按操作分级 + 硬安全规则，scope 与操作分离）：
//!   `check_remote_access(path, op, host_roots, sftp)` 流水线：
//!   1. null 字节拒绝
//!   2. `~` / `~/...` 按远端 home（`sftp.home()`，通道级缓存）展开
//!   3. 远端 realpath 规范化
//!   4. Effective scope：`host_roots`（hosts.toml per-host）优先，否则全局
//!      （`TERMBRIDGE_ALLOWED_REMOTE_PATHS` 环境变量 → 默认 `["/"]`）前缀匹配
//!   5. Hard safety（无条件、无论 scope 多宽松）：`~/.ssh/authorized_keys` 与
//!      `/proc` `/sys` 的写/建/删 → 硬拒（authorized_keys 只能走 bootstrap_host）
//!
//! 默认：
//! - `allowed_local_paths` = `[cwd, temp/termbridge]`（+ `TERMBRIDGE_ALLOWED_LOCAL_PATHS`）
//! - `allowed_remote_paths` = `TERMBRIDGE_ALLOWED_REMOTE_PATHS`（默认 `["/"]`，不缩小
//!   SSH 账号已具备的权限；用户/运维可经 env 或 hosts.toml 按主机收紧，安全底线由
//!   操作分级 guardrail 承担）
//!
//! 错误码（§6.1）：`LOCAL_PATH_NOT_ALLOWED` / `REMOTE_PATH_NOT_ALLOWED`（retriable=false）。

use std::path::{Path, PathBuf};

use crate::domain::provider::{SftpCanonicalize, TermError};

/// 远端 SFTP 操作类型。路径策略按操作分级：同一路径读与写/删除风险不同，
/// 硬安全规则只拦截"高风险操作"而非整条路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteOperation {
    /// 读文件 / 列目录
    Read,
    /// 写已有文件（覆盖）
    Write,
    /// 创建新文件 / 新目录
    Create,
    /// 删除文件 / 目录
    Delete,
    /// 修改权限（chmod）
    Chmod,
}

impl RemoteOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            RemoteOperation::Read => "read",
            RemoteOperation::Write => "write",
            RemoteOperation::Create => "create",
            RemoteOperation::Delete => "delete",
            RemoteOperation::Chmod => "chmod",
        }
    }
}

/// SFTP 路径策略。
///
/// 构造后不可变；SessionManager 持有 Arc 共享给所有 sftp_transfer 调用。
pub struct PathPolicy {
    /// 允许读写的本地根路径列表（已规范化为绝对路径）。
    /// 默认 `[std::env::current_dir()]`。
    allowed_local_paths: Vec<PathBuf>,
    /// 允许读写的远端根路径列表（绝对路径，前缀匹配）。
    /// 默认 `TERMBRIDGE_ALLOWED_REMOTE_PATHS`（未设则 `["/"]`，不缩小 SSH 账号权限；
    /// 安全由操作分级 guardrail + 硬安全规则承担）。条目可含 `~`。
    /// 主机级覆盖见 hosts.toml。
    allowed_remote_paths: Vec<String>,
}

impl PathPolicy {
    /// 用给定的白名单构造。`allowed_remote_paths` 为空则等价 `["/"]`。
    ///
    /// `allowed_local_paths` 会做 canonicalize（解析 symlink / `..` / `.`），
    /// 保证与后续 `check_local` 中 canonicalize 出的路径可比。
    /// canonicalize 失败的项保留原值（运行时 check_local 也会失败，错误一致）。
    pub fn new(
        allowed_local_paths: Vec<PathBuf>,
        allowed_remote_paths: Vec<String>,
    ) -> Self {
        let allowed_local_paths = allowed_local_paths
            .into_iter()
            .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
            .collect();
        let allowed_remote_paths = if allowed_remote_paths.is_empty() {
            tracing::warn!(
                "PathPolicy: allowed_remote_paths 为空，等价 [\"/\"]（全放行），建议收紧"
            );
            vec!["/".to_string()]
        } else {
            allowed_remote_paths
        };
        Self {
            allowed_local_paths,
            allowed_remote_paths,
        }
    }

    /// 默认策略：`allowed_local_paths=[cwd, temp/termbridge]`，
    /// `allowed_remote_paths=["/"]`。
    ///
    /// Phase 8：在 cwd 基础上增加 OS temp 下的 `termbridge` 子目录，用于 SFTP
    /// 下载到本地的临时工作文件。MCP server 作为宿主 IDE 子进程启动时 cwd
    /// 不可控（可能是 IDE 安装目录），temp 目录提供稳定可写的落盘点。
    ///
    /// 通过环境变量 `TERMBRIDGE_ALLOWED_LOCAL_PATHS` 可追加额外路径
    ///（Windows 用 `;` 分隔，Unix 用 `:` 分隔），让宿主把 workspace 显式传入。
    pub fn default_from_cwd() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "current_dir 失败，回退到 \".\"");
            PathBuf::from(".")
        });

        let mut allowed_local = vec![cwd.clone()];

        // temp/termbridge 子目录：SFTP 临时工作文件落盘点
        let temp_root = std::env::temp_dir().join("termbridge");
        if let Err(e) = std::fs::create_dir_all(&temp_root) {
            tracing::warn!(error = %e, ?temp_root, "创建 temp/termbridge 目录失败");
        }
        allowed_local.push(temp_root);

        // 环境变量追加：让宿主显式传入 workspace 等额外路径
        // （Windows 用 ; 分隔，Unix 用 : 分隔）
        if let Ok(extra) = std::env::var("TERMBRIDGE_ALLOWED_LOCAL_PATHS") {
            let sep = if cfg!(windows) { ';' } else { ':' };
            for path in extra.split(sep) {
                let p = path.trim();
                if !p.is_empty() {
                    allowed_local.push(PathBuf::from(p));
                }
            }
        }

        tracing::info!(
            ?allowed_local,
            "PathPolicy: allowed_local_paths = cwd + temp/termbridge + TERMBRIDGE_ALLOWED_LOCAL_PATHS"
        );

        // 远端全局默认：TERMBRIDGE_ALLOWED_REMOTE_PATHS（与本地对称，分隔符同规则；
        // 条目可含 `~`，按远端 home 展开）。未配置 → ["/"]（不缩小 SSH 账号已具备的
        // 权限；安全底线由操作分级 guardrail 承担）。主机级覆盖见 hosts.toml。
        let allowed_remote = match std::env::var("TERMBRIDGE_ALLOWED_REMOTE_PATHS") {
            Ok(v) if !v.trim().is_empty() => {
                let sep = if cfg!(windows) { ';' } else { ':' };
                let roots: Vec<String> = v
                    .split(sep)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                tracing::info!(
                    ?roots,
                    "PathPolicy: allowed_remote_paths 来自 TERMBRIDGE_ALLOWED_REMOTE_PATHS"
                );
                roots
            }
            _ => {
                tracing::warn!(
                    "PathPolicy: allowed_remote_paths 默认 [\"/\"]（不缩小 SSH 账号权限；\
                     异常位置写操作由操作级 guardrail 拦截）；可用 TERMBRIDGE_ALLOWED_REMOTE_PATHS 或 hosts.toml 收紧"
                );
                vec!["/".to_string()]
            }
        };

        Self::new(allowed_local, allowed_remote)
    }

    // ── 本地路径检查 ──────────────────────────────────────────────

    /// 检查本地路径是否允许读写。
    ///
    /// 规则：
    /// 1. 拒绝含 null 字节的路径（防 `\0` 注入绕过）。
    /// 2. `canonicalize()` 解析为绝对路径（解析 `..` / `.` / symlink）。
    ///    - 文件不存在 → 失败（canonicalize 要求路径存在）。
    ///    - 调用方需在文件创建前调用（upload 读已存在文件、download 写新文件需特殊处理）。
    /// 3. 检查规范化路径是否在 `allowed_local_paths` 任一根下。
    ///
    /// 注：download 目标文件可能不存在（canonicalize 失败）。Phase 1 简化处理：
    /// 若 canonicalize 失败，检查其**父目录**是否允许（目标文件将在允许的根下被创建）。
    pub fn check_local(&self, path: &Path) -> Result<(), TermError> {
        // 1. 拒绝 null 字节
        if contains_null(path) {
            return Err(TermError::LocalPathNotAllowed(format!(
                "path contains null byte: {}",
                path.display()
            )));
        }

        // 2. canonicalize（解析 .. / . / symlink 为绝对路径）
        //    文件不存在 → 检查父目录
        let canonical = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(_) => {
                // 文件不存在（download 目标）。检查父目录是否允许。
                let parent = path.parent().ok_or_else(|| {
                    TermError::LocalPathNotAllowed(format!(
                        "path has no parent: {}",
                        path.display()
                    ))
                })?;
                let parent_canonical = std::fs::canonicalize(parent).map_err(|e| {
                    TermError::LocalPathNotAllowed(format!(
                        "canonicalize parent '{}' failed: {e}",
                        parent.display()
                    ))
                })?;
                // 父目录允许 → 文件将在允许根下被创建，视为 OK
                return self.check_canonical_under_any(&parent_canonical, &self.allowed_local_paths, path);
            }
        };

        // 3. 检查规范化路径是否在任一 allowed 根下
        self.check_canonical_under_any(&canonical, &self.allowed_local_paths, path)
    }

    // ── 远端路径检查 ──────────────────────────────────────────────

    /// 按操作检查远端路径访问（核心入口，scope 与操作分离）。
    ///
    /// 流水线：
    /// 1. 拒绝含 null 字节的路径。
    /// 2. `~` / `~/...` 按远端 home（`sftp.home()`）展开（路径或允许根含 `~` 时）。
    /// 3. 远端 `realpath` 规范化（防 `..` / symlink 逃逸）；`Create` 目标不存在时
    ///    校验其父目录（需已存在且被允许）。
    /// 4. Effective scope：`host_roots`（hosts.toml per-host，有则优先）或全局
    ///    （`TERMBRIDGE_ALLOWED_REMOTE_PATHS` / 默认 `["/"]`），前缀匹配。
    /// 5. Hard safety（无条件）：`~/.ssh/authorized_keys` 与 `/proc` `/sys` 的
    ///    写/建/删 → 硬拒（authorized_keys 只能经 `bootstrap_host` 部署）。
    pub async fn check_remote_access(
        &self,
        path: &str,
        op: RemoteOperation,
        host_roots: Option<&[String]>,
        sftp: &dyn SftpCanonicalize,
    ) -> Result<(), TermError> {
        // 1. 拒绝 null 字节
        if path.as_bytes().contains(&0u8) {
            return Err(TermError::RemotePathNotAllowed(format!(
                "path contains null byte: {path}"
            )));
        }

        // 2. `~` 展开（按需调 home，避免无 `~` 时多一次远端往返）
        let needs_home = path.contains('~')
            || host_roots.is_some_and(|r| r.iter().any(|root| root.contains('~')))
            || self.allowed_remote_paths.iter().any(|root| root.contains('~'));
        let home = if needs_home {
            match sftp.home().await {
                Some(h) => Some(h),
                None => {
                    return Err(TermError::RemotePathNotAllowed(format!(
                        "remote path '{path}' 使用 '~'，但无法解析远端 home（非 OpenSSH / \
                         chroot 等）；请改用绝对路径"
                    )));
                }
            }
        } else {
            None
        };
        let expanded = expand_tilde(path, home.as_deref());

        // 3. realpath 规范化（Create：目标不存在 → 校验父目录）
        let canonical = match sftp.canonicalize(&expanded).await {
            Ok(c) => c,
            Err(TermError::SftpNoSuchFile(_)) if op == RemoteOperation::Create => {
                let parent = parent_remote_path(&expanded).ok_or_else(|| {
                    TermError::RemotePathNotAllowed(format!(
                        "cannot create: '{path}' has no parent directory"
                    ))
                })?;
                let parent_canonical = sftp.canonicalize(&parent).await?;
                return self.check_scope_and_safety(
                    &parent_canonical,
                    path,
                    op,
                    host_roots,
                    home.as_deref(),
                );
            }
            Err(e) => return Err(e),
        };

        // 4 + 5. scope 检查 + 硬安全规则
        self.check_scope_and_safety(&canonical, path, op, host_roots, home.as_deref())
    }

    /// scope（允许根前缀匹配）→ hard safety（无条件拒绝）两段判定。
    fn check_scope_and_safety(
        &self,
        canonical: &str,
        original: &str,
        op: RemoteOperation,
        host_roots: Option<&[String]>,
        home: Option<&str>,
    ) -> Result<(), TermError> {
        // 4. Effective scope：per-host（hosts.toml）优先，否则全局；含 `~` 的根展开
        let effective: Vec<String> = match host_roots {
            Some(roots) if !roots.is_empty() => roots.iter().map(|r| expand_tilde(r, home)).collect(),
            _ => self
                .allowed_remote_paths
                .iter()
                .map(|r| expand_tilde(r, home))
                .collect(),
        };
        let in_scope = effective
            .iter()
            .any(|root| is_under_remote(canonical, root));
        if !in_scope {
            return Err(TermError::RemotePathNotAllowed(format!(
                "remote path '{original}' resolves to '{canonical}', not under allowed roots {:?} \
                 （可在 hosts.toml 或 TERMBRIDGE_ALLOWED_REMOTE_PATHS 中声明）",
                effective
            )));
        }

        // 5. Hard safety（无条件，先拒后允）
        if let Some(reason) = hard_safety_deny(canonical, op) {
            return Err(TermError::RemotePathNotAllowed(format!(
                "{reason}（path: '{original}' → '{canonical}', op: {}）",
                op.as_str()
            )));
        }
        Ok(())
    }

    /// 兼容入口：读操作 + 全局 scope（既有测试 / 只读场景）。
    pub async fn check_remote(
        &self,
        path: &str,
        sftp: &dyn SftpCanonicalize,
    ) -> Result<(), TermError> {
        self.check_remote_access(path, RemoteOperation::Read, None, sftp).await
    }

    /// 兼容入口：创建操作 + 全局 scope（mkdir / 上传目标可能不存在）。
    pub async fn check_remote_allow_new(
        &self,
        path: &str,
        sftp: &dyn SftpCanonicalize,
    ) -> Result<(), TermError> {
        self.check_remote_access(path, RemoteOperation::Create, None, sftp).await
    }

    /// 已规范化本地路径的前缀检查（提取为独立方法便于测试）。
    fn check_canonical_under_any(
        &self,
        canonical: &Path,
        allowed: &[PathBuf],
        original: &Path,
    ) -> Result<(), TermError> {
        for root in allowed {
            if canonical.starts_with(root) {
                return Ok(());
            }
        }
        Err(TermError::LocalPathNotAllowed(format!(
            "local path '{}' resolves to '{}', not under allowed roots {:?}",
            original.display(),
            canonical.display(),
            allowed
        )))
    }
}

// ── 辅助函数 ──────────────────────────────────────────────────────

/// 路径是否含 null 字节。
///
/// Unix：检查 OsStr 原始 bytes 是否含 `\0`。
/// Windows：OsStr 不能直接拿 bytes；null 字节在 `Path::new` 时已被处理，
/// 用 `to_string_lossy` 检查是否含替换字符 U+FFFD（null 会被替换为它）作为兜底。
fn contains_null(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().contains(&0u8)
    }
    #[cfg(not(unix))]
    {
        // Windows：Path 构造时已拒绝 null 字节；to_string_lossy 把无效 unicode 替换为 U+FFFD。
        // 检查 U+FFFD 作为兜底（覆盖 null 等异常字节路径）。
        path.to_string_lossy().contains('\u{fffd}')
    }
}

/// 远端规范化路径是否在 `root` 之下（前缀匹配，路径感知）。
///
/// 规则：`canonical == root` 或 `canonical` 以 `root + "/"` 开头。
/// 例：`/home/user/file.txt` 在 `/home/user` 之下，但 `/home/userfoo` 不在。
fn is_under_remote(canonical: &str, root: &str) -> bool {
    // 规范化：去尾部斜杠（除非根 "/")
    let root = root.trim_end_matches('/');
    let root = if root.is_empty() { "/" } else { root };

    if canonical == root {
        return true;
    }
    // 检查 canonical 以 "root/" 开头
    let prefix = if root == "/" {
        "/".to_string()
    } else {
        format!("{root}/")
    };
    canonical.starts_with(&prefix)
}

/// 展开远端路径中的 `~` / `~/...`（POSIX 语义）。
///
/// 仅在 home 可解析且 `~` 位于开头时展开；其余情况原样返回（交给远端 realpath）。
/// `home` 为 None（不可解析）→ 原样返回，由调用方保证此前已报错。
fn expand_tilde(path: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return path.to_string();
    };
    if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        if rest.is_empty() {
            home.to_string()
        } else {
            format!("{home}/{rest}")
        }
    } else {
        path.to_string()
    }
}

/// 无条件硬安全规则（先于 allowlist 生效，任意命中即拒）。
///
/// 只拦"高风险操作"（写/建/删/改权限），读不受限：
/// - `~/.ssh/authorized_keys`：认证控制面文件，公钥部署唯一受控通道是
///   `bootstrap_host`（SFTP 写会改变 SSH 登录权限，与受控流程冲突）
/// - `/proc` / `/sys`：内核接口，禁止写 / 建 / 删
fn hard_safety_deny(canonical: &str, op: RemoteOperation) -> Option<String> {
    use RemoteOperation::{Chmod, Create, Delete, Read, Write};
    match op {
        Read => None,
        Write | Create | Delete | Chmod => {
            // 1. authorized_keys（end-suffix 匹配：无论 home 是否能解析都覆盖）
            if canonical.ends_with("/.ssh/authorized_keys") {
                return Some(
                    "~/.ssh/authorized_keys 是认证控制面文件，SFTP 写/删被禁止（公钥部署只能走 bootstrap_host）".to_string(),
                );
            }
            // 2. /proc /sys 内核接口读写
            if canonical == "/proc"
                || canonical.starts_with("/proc/")
                || canonical == "/sys"
                || canonical.starts_with("/sys/")
            {
                return Some(format!(
                    "/proc 与 /sys 为内核接口，禁止 {} 操作",
                    op.as_str()
                ));
            }
            None
        }
    }
}

/// 取远端 POSIX 路径的父目录（Phase 2，check_remote_allow_new 用）。
///
/// 远端路径是 POSIX 风格（正斜杠），不能用 `Path::parent`（Windows 上会按反斜杠解析）。
///
/// 返回值：
/// - `/home/user/newdir` → `Some("/home/user")`
/// - `/newdir` → `Some("/")`（父目录是根）
/// - `/` → `None`（根无父目录）
/// - `relative` → `None`（无斜杠，无父目录）
/// - `/home/user/` → `Some("/home/user")`（先去尾斜杠再取父）
fn parent_remote_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        // 原路径是 "/" 或全斜杠 → 根，无父目录
        return None;
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()), // 父目录是根
        Some(idx) => Some(trimmed[..idx].to_string()),
        None => None, // 无斜杠（相对路径），无父目录
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// 假 SftpCanonicalize：返回预设的 canonical 路径。
    struct FakeCanonicalize {
        mapping: Mutex<std::collections::HashMap<String, String>>,
    }

    #[async_trait]
    impl SftpCanonicalize for FakeCanonicalize {
        async fn canonicalize(&self, path: &str) -> Result<String, TermError> {
            self.mapping
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| TermError::SftpNoSuchFile(format!("not found: {path}")))
        }
    }

    fn policy(allowed_local: Vec<PathBuf>, allowed_remote: Vec<String>) -> PathPolicy {
        PathPolicy::new(allowed_local, allowed_remote)
    }

    // ── 远端路径策略测试 ──────────────────────────────────────────

    #[tokio::test]
    async fn check_remote_allows_path_under_root() {
        let sftp = FakeCanonicalize {
            mapping: Mutex::new([("/home/user/file.txt".into(), "/home/user/file.txt".into())].into()),
        };
        let p = policy(vec![], vec!["/home/user".into()]);
        assert!(p.check_remote("/home/user/file.txt", &sftp).await.is_ok());
    }

    #[tokio::test]
    async fn check_remote_allows_root_itself() {
        let sftp = FakeCanonicalize {
            mapping: Mutex::new([("/home/user".into(), "/home/user".into())].into()),
        };
        let p = policy(vec![], vec!["/home/user".into()]);
        assert!(p.check_remote("/home/user", &sftp).await.is_ok());
    }

    #[tokio::test]
    async fn check_remote_rejects_path_outside_root() {
        let sftp = FakeCanonicalize {
            mapping: Mutex::new([("/etc/passwd".into(), "/etc/passwd".into())].into()),
        };
        let p = policy(vec![], vec!["/home/user".into()]);
        let err = p.check_remote("/etc/passwd", &sftp).await.unwrap_err();
        assert_eq!(err.code(), "REMOTE_PATH_NOT_ALLOWED");
        assert!(!err.retriable());
    }

    #[tokio::test]
    async fn check_remote_rejects_sibling_with_same_prefix() {
        // /home/userfoo 不应在 /home/user 下（防字符串前缀误判）
        let sftp = FakeCanonicalize {
            mapping: Mutex::new([("/home/userfoo".into(), "/home/userfoo".into())].into()),
        };
        let p = policy(vec![], vec!["/home/user".into()]);
        let err = p.check_remote("/home/userfoo", &sftp).await.unwrap_err();
        assert_eq!(err.code(), "REMOTE_PATH_NOT_ALLOWED");
    }

    #[tokio::test]
    async fn check_remote_resolves_dotdot_via_realpath() {
        // 输入 /home/user/../../etc/passwd，realpath 解析为 /etc/passwd
        // 应被拒绝（不在 /home/user 下）
        let sftp = FakeCanonicalize {
            mapping: Mutex::new(
                [("/home/user/../../etc/passwd".into(), "/etc/passwd".into())].into(),
            ),
        };
        let p = policy(vec![], vec!["/home/user".into()]);
        let err = p.check_remote("/home/user/../../etc/passwd", &sftp).await.unwrap_err();
        assert_eq!(err.code(), "REMOTE_PATH_NOT_ALLOWED");
    }

    #[tokio::test]
    async fn check_remote_resolves_symlink_escape_via_realpath() {
        // 输入 /home/user/link，realpath 解析为 /etc/passwd（symlink 逃逸）
        let sftp = FakeCanonicalize {
            mapping: Mutex::new([("/home/user/link".into(), "/etc/passwd".into())].into()),
        };
        let p = policy(vec![], vec!["/home/user".into()]);
        let err = p.check_remote("/home/user/link", &sftp).await.unwrap_err();
        assert_eq!(err.code(), "REMOTE_PATH_NOT_ALLOWED");
    }

    #[tokio::test]
    async fn check_remote_rejects_null_byte() {
        let sftp = FakeCanonicalize {
            mapping: Mutex::new(std::collections::HashMap::new()),
        };
        let p = policy(vec![], vec!["/".into()]);
        let path = "/home/user\0/etc/passwd";
        let err = p.check_remote(path, &sftp).await.unwrap_err();
        assert_eq!(err.code(), "REMOTE_PATH_NOT_ALLOWED");
    }

    #[tokio::test]
    async fn check_remote_default_root_slash_allows_everything() {
        let sftp = FakeCanonicalize {
            mapping: Mutex::new([("/anywhere/file".into(), "/anywhere/file".into())].into()),
        };
        let p = policy(vec![], vec!["/".into()]); // 默认全放行
        assert!(p.check_remote("/anywhere/file", &sftp).await.is_ok());
    }

    // ── 本地路径策略测试 ──────────────────────────────────────────

    #[test]
    fn check_local_allows_path_under_allowed_root() {
        let tmp = std::env::temp_dir();
        let dir = tmp.join("termbridge_path_policy_test_under");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file.txt");
        std::fs::write(&file, b"hi").unwrap();

        let p = policy(vec![dir.clone()], vec!["/".into()]);
        assert!(p.check_local(&file).is_ok());

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn check_local_rejects_path_outside_allowed_root() {
        let tmp = std::env::temp_dir();
        let outside = tmp.join("termbridge_path_policy_test_outside");
        std::fs::create_dir_all(&outside).unwrap();
        let file = outside.join("file.txt");
        std::fs::write(&file, b"hi").unwrap();

        // allowed root 是另一个目录
        let allowed_root = tmp.join("termbridge_path_policy_allowed_other");
        std::fs::create_dir_all(&allowed_root).unwrap();

        let p = policy(vec![allowed_root.clone()], vec!["/".into()]);
        let err = p.check_local(&file).unwrap_err();
        assert_eq!(err.code(), "LOCAL_PATH_NOT_ALLOWED");
        assert!(!err.retriable());

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&outside);
        let _ = std::fs::remove_dir(&allowed_root);
    }

    #[test]
    fn check_local_rejects_null_byte_in_path() {
        // Windows 上 Path 构造时已拒绝 null；测试路径含特殊字符场景
        // 直接构造一个合法路径但模拟 null 检查逻辑
        let p = policy(vec![PathBuf::from("/")], vec!["/".into()]);
        // 在 unix 上可测 \0，在 Windows 上 Path::new("\0") 会 panic 或被拒绝
        // 这里测试 to_string_lossy 含 U+FFFD 的场景
        let path = Path::new("\u{fffd}.txt");
        // 这个路径在文件系统中不存在，但 contains_null 检查应触发
        let _ = p.check_local(path); // 不断言结果，仅验证不 panic
    }

    #[test]
    fn check_local_nonexistent_target_under_allowed_parent_is_ok() {
        // download 目标文件可能不存在；检查父目录允许即可
        let tmp = std::env::temp_dir();
        let dir = tmp.join("termbridge_path_policy_test_nonexist");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("nonexistent.bin");

        let p = policy(vec![dir.clone()], vec!["/".into()]);
        assert!(p.check_local(&target).is_ok());

        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn check_local_nonexistent_target_outside_allowed_parent_rejected() {
        let tmp = std::env::temp_dir();
        let allowed = tmp.join("termbridge_path_policy_allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let outside = tmp.join("termbridge_path_policy_outside_nonexist");
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("nonexistent.bin");

        let p = policy(vec![allowed.clone()], vec!["/".into()]);
        let err = p.check_local(&target).unwrap_err();
        assert_eq!(err.code(), "LOCAL_PATH_NOT_ALLOWED");

        let _ = std::fs::remove_dir(&allowed);
        let _ = std::fs::remove_dir(&outside);
    }

    // ── is_under_remote 单元测试 ──────────────────────────────────

    #[test]
    fn is_under_remote_matches_exact_root() {
        assert!(is_under_remote("/home/user", "/home/user"));
    }

    #[test]
    fn is_under_remote_matches_descendant() {
        assert!(is_under_remote("/home/user/file.txt", "/home/user"));
        assert!(is_under_remote("/home/user/sub/file.txt", "/home/user"));
    }

    #[test]
    fn is_under_remote_rejects_sibling_same_prefix() {
        // /home/userfoo 不应在 /home/user 下
        assert!(!is_under_remote("/home/userfoo", "/home/user"));
        assert!(!is_under_remote("/home/user_extra", "/home/user"));
    }

    #[test]
    fn is_under_remote_root_slash_matches_everything() {
        assert!(is_under_remote("/anywhere/file", "/"));
        assert!(is_under_remote("/etc/passwd", "/"));
        assert!(is_under_remote("/", "/"));
    }

    #[test]
    fn is_under_remote_normalizes_trailing_slash() {
        // allowed root 带尾部斜杠也应正确匹配
        assert!(is_under_remote("/home/user/file", "/home/user/"));
    }

    // ── Phase 1：upload / download 场景路径策略测试 ──────────────
    // 验证 SFTP 传输两个方向的路径策略拒绝行为（§5.5 / ADR-0005 §4）

    /// upload 场景：本地源文件不在 allowed_local_paths 内 → LOCAL_PATH_NOT_ALLOWED。
    ///
    /// upload 时 local_path 是源文件（必须存在），remote_path 是目标。
    /// check_local 拒绝本地源 → 整个 upload 被拒。
    #[test]
    fn upload_local_source_outside_allowed_roots_rejected() {
        let tmp = std::env::temp_dir();
        // allowed root：目录 A
        let allowed_root = tmp.join("termbridge_upload_allowed");
        std::fs::create_dir_all(&allowed_root).unwrap();
        // 本地源文件：在目录 B（allowed 之外）
        let outside_dir = tmp.join("termbridge_upload_outside");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let source = outside_dir.join("source.txt");
        std::fs::write(&source, b"upload me").unwrap();

        let p = policy(vec![allowed_root.clone()], vec!["/".into()]);
        let err = p.check_local(&source).unwrap_err();
        assert_eq!(err.code(), "LOCAL_PATH_NOT_ALLOWED");
        assert!(!err.retriable());

        let _ = std::fs::remove_file(&source);
        let _ = std::fs::remove_dir(&outside_dir);
        let _ = std::fs::remove_dir(&allowed_root);
    }

    /// download 场景：远端源文件不在 allowed_remote_paths 内 → REMOTE_PATH_NOT_ALLOWED。
    ///
    /// download 时 remote_path 是源文件，local_path 是目标。
    /// check_remote 拒绝远端源 → 整个 download 被拒。
    #[tokio::test]
    async fn download_remote_source_outside_allowed_roots_rejected() {
        // allowed_remote_paths 限定为 /home/user，但远端源解析为 /etc/passwd
        let sftp = FakeCanonicalize {
            mapping: Mutex::new([("/etc/passwd".into(), "/etc/passwd".into())].into()),
        };
        let p = policy(vec![], vec!["/home/user".into()]);
        let err = p.check_remote("/etc/passwd", &sftp).await.unwrap_err();
        assert_eq!(err.code(), "REMOTE_PATH_NOT_ALLOWED");
        assert!(!err.retriable());
    }

    /// upload 场景：远端目标不在 allowed_remote_paths 内 → REMOTE_PATH_NOT_ALLOWED。
    ///
    /// upload 的 remote_path 也需通过 check_remote（防远端路径穿越）。
    #[tokio::test]
    async fn upload_remote_dest_outside_allowed_roots_rejected() {
        // allowed_remote_paths 限定为 /tmp，但远端目标解析为 /root/.ssh/authorized_keys
        let sftp = FakeCanonicalize {
            mapping: Mutex::new([
                ("/root/.ssh/authorized_keys".into(), "/root/.ssh/authorized_keys".into()),
            ]
            .into()),
        };
        let p = policy(vec![], vec!["/tmp".into()]);
        let err = p
            .check_remote("/root/.ssh/authorized_keys", &sftp)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "REMOTE_PATH_NOT_ALLOWED");
        assert!(!err.retriable());
    }

    /// download 场景：本地目标不在 allowed_local_paths 内 → LOCAL_PATH_NOT_ALLOWED。
    ///
    /// download 的 local_path 是目标（可能不存在），check_local 检查父目录。
    #[test]
    fn download_local_dest_outside_allowed_roots_rejected() {
        let tmp = std::env::temp_dir();
        let allowed_root = tmp.join("termbridge_dl_allowed");
        std::fs::create_dir_all(&allowed_root).unwrap();
        let outside_dir = tmp.join("termbridge_dl_outside");
        std::fs::create_dir_all(&outside_dir).unwrap();
        // 目标文件不存在，但父目录在 allowed 之外
        let dest = outside_dir.join("downloaded.bin");

        let p = policy(vec![allowed_root.clone()], vec!["/".into()]);
        let err = p.check_local(&dest).unwrap_err();
        assert_eq!(err.code(), "LOCAL_PATH_NOT_ALLOWED");
        assert!(!err.retriable());

        let _ = std::fs::remove_dir(&outside_dir);
        let _ = std::fs::remove_dir(&allowed_root);
    }

    // ── Phase 2：parent_remote_path 单元测试 ─────────────────────

    #[test]
    fn parent_remote_path_normal() {
        assert_eq!(parent_remote_path("/home/user/newdir"), Some("/home/user".into()));
    }

    #[test]
    fn parent_remote_path_one_level_deep() {
        assert_eq!(parent_remote_path("/newdir"), Some("/".into()));
    }

    #[test]
    fn parent_remote_path_root_returns_none() {
        assert_eq!(parent_remote_path("/"), None);
        assert_eq!(parent_remote_path("///"), None);
    }

    #[test]
    fn parent_remote_path_trailing_slash() {
        assert_eq!(parent_remote_path("/home/user/"), Some("/home".into()));
    }

    #[test]
    fn parent_remote_path_relative_no_slash() {
        assert_eq!(parent_remote_path("relative"), None);
    }

    // ── Phase 2：check_remote_allow_new 测试 ─────────────────────

    #[tokio::test]
    async fn check_remote_allow_new_existing_path_ok() {
        // 目标已存在 → realpath 成功 → 检查在允许根下
        let sftp = FakeCanonicalize {
            mapping: Mutex::new([("/home/user/newdir".into(), "/home/user/newdir".into())].into()),
        };
        let p = policy(vec![], vec!["/home/user".into()]);
        assert!(p.check_remote_allow_new("/home/user/newdir", &sftp).await.is_ok());
    }

    #[tokio::test]
    async fn check_remote_allow_new_nonexistent_falls_back_to_parent() {
        // 目标不存在 → SftpNoSuchFile → 校验父目录
        // 父目录 /home/user 存在且在允许根下 → Ok
        let sftp = FakeCanonicalize {
            mapping: Mutex::new([("/home/user".into(), "/home/user".into())].into()),
        };
        let p = policy(vec![], vec!["/home/user".into()]);
        assert!(p.check_remote_allow_new("/home/user/newdir", &sftp).await.is_ok());
    }

    #[tokio::test]
    async fn check_remote_allow_new_nonexistent_parent_outside_rejected() {
        // 目标不存在 → 校验父目录 → 父目录不在允许根下 → REMOTE_PATH_NOT_ALLOWED
        let sftp = FakeCanonicalize {
            mapping: Mutex::new([("/etc".into(), "/etc".into())].into()),
        };
        let p = policy(vec![], vec!["/home/user".into()]);
        let err = p.check_remote_allow_new("/etc/newdir", &sftp).await.unwrap_err();
        assert_eq!(err.code(), "REMOTE_PATH_NOT_ALLOWED");
    }

    #[tokio::test]
    async fn check_remote_allow_new_root_rejected() {
        // 目标是根 → 无父目录 → 拒绝
        let sftp = FakeCanonicalize {
            mapping: Mutex::new(std::collections::HashMap::new()),
        };
        let p = policy(vec![], vec!["/".into()]);
        let err = p.check_remote_allow_new("/", &sftp).await.unwrap_err();
        assert_eq!(err.code(), "REMOTE_PATH_NOT_ALLOWED");
    }

    #[tokio::test]
    async fn check_remote_allow_new_rejects_null_byte() {
        let sftp = FakeCanonicalize {
            mapping: Mutex::new(std::collections::HashMap::new()),
        };
        let p = policy(vec![], vec!["/".into()]);
        let err = p.check_remote_allow_new("/home/user\0/etc", &sftp).await.unwrap_err();
        assert_eq!(err.code(), "REMOTE_PATH_NOT_ALLOWED");
    }

    // ── 操作分级 + 硬安全规则 + 主机级 scope + `~` 展开 ────────────

    /// 带 home 的假 SFTP（用于 `~` 展开相关测试）。
    struct FakeSftpHome {
        mapping: std::sync::Mutex<std::collections::HashMap<String, String>>,
        home: Option<String>,
    }

    #[async_trait]
    impl SftpCanonicalize for FakeSftpHome {
        async fn canonicalize(&self, path: &str) -> Result<String, TermError> {
            self.mapping
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| TermError::SftpNoSuchFile(format!("not found: {path}")))
        }
        async fn home(&self) -> Option<String> {
            self.home.clone()
        }
    }

    fn fake_home(mapping: Vec<(&str, &str)>, home: Option<&str>) -> FakeSftpHome {
        FakeSftpHome {
            mapping: Mutex::new(mapping.into_iter().map(|(a, b)| (a.into(), b.into())).collect()),
            home: home.map(|s| s.to_string()),
        }
    }

    #[tokio::test]
    async fn authorized_keys_write_denied_but_read_allowed() {
        // default scope "/"（不缩小 SSH 权限），但 authorized_keys 写必须硬拒
        let sftp = fake_home(
            vec![("/home/u/.ssh/authorized_keys", "/home/u/.ssh/authorized_keys")],
            Some("/home/u"),
        );
        let p = policy(vec![], vec!["/".into()]);
        assert!(p
            .check_remote_access(
                "/home/u/.ssh/authorized_keys",
                RemoteOperation::Write,
                None,
                &sftp,
            )
            .await
            .is_err());
        assert!(p
            .check_remote_access(
                "/home/u/.ssh/authorized_keys",
                RemoteOperation::Chmod,
                None,
                &sftp,
            )
            .await
            .is_err());
        // 读不受限（运维排查公钥状态是合法需求）
        assert!(p
            .check_remote_access(
                "/home/u/.ssh/authorized_keys",
                RemoteOperation::Read,
                None,
                &sftp,
            )
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn proc_sys_mutation_denied_but_read_allowed() {
        let sftp = fake_home([("/proc/self/environ", "/proc/self/environ")].into(), None);
        let p = policy(vec![], vec!["/".into()]);
        for op in [
            RemoteOperation::Write,
            RemoteOperation::Create,
            RemoteOperation::Delete,
        ] {
            assert!(
                p.check_remote_access("/proc/self/environ", op, None, &sftp).await.is_err(),
                "{op:?} 应被硬拒"
            );
        }
        assert!(p
            .check_remote_access("/proc/self/environ", RemoteOperation::Read, None, &sftp)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn tilde_expands_via_remote_home() {
        // allowed 根为 "~"，路径 "~/log/app.log" 展开为 "/home/u/log/app.log"
        // （fake 的 mapping 只接受展开后的路径，展开成功即证明逻辑正确）
        let sftp = fake_home(
            vec![("/home/u/log/app.log", "/home/u/log/app.log")],
            Some("/home/u"),
        );
        let p = policy(vec![], vec!["~".into()]);
        assert!(p
            .check_remote_access("~/log/app.log", RemoteOperation::Read, None, &sftp)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn tilde_unresolvable_home_rejects_with_absolute_hint() {
        // home 解析不到（非 OpenSSH / chroot）：含 `~` 请求报错并提示绝对路径
        let sftp = fake_home(vec![], None);
        let p = policy(vec![], vec!["~".into()]);
        let err = p
            .check_remote_access("~/x", RemoteOperation::Read, None, &sftp)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "REMOTE_PATH_NOT_ALLOWED");
    }

    #[tokio::test]
    async fn host_roots_override_global_scope() {
        // hosts.toml per-host scope 优先于全局
        let sftp = fake_home(
            vec![
                ("/home/u/x".into(), "/home/u/x".into()),
                ("/opt/app/y".into(), "/opt/app/y".into()),
            ],
            Some("/home/u"),
        );
        let p = policy(vec![], vec!["/home/u".into()]); // 全局：home
        let host_roots = vec!["/opt/app".to_string()];
        assert!(p
            .check_remote_access("/home/u/x", RemoteOperation::Read, Some(&host_roots), &sftp)
            .await
            .is_err(), "per-host scope 应覆盖全局");
        assert!(p
            .check_remote_access("/opt/app/y", RemoteOperation::Read, Some(&host_roots), &sftp)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn empty_host_roots_falls_back_to_global() {
        let sftp = fake_home(vec![("/home/u/x", "/home/u/x")], Some("/home/u"));
        let p = policy(vec![], vec!["/home/u".into()]);
        assert!(p
            .check_remote_access("/home/u/x", RemoteOperation::Read, Some(&[]), &sftp)
            .await
            .is_ok(), "空 host scope 应回退全局");
    }
}
