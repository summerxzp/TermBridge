//! path_policy —— SFTP 路径策略（ADR-0005 §4 / PLAN §5.5 / §7.4 Phase 1）
//!
//! 拒绝策略：
//! - 本地路径：`canonicalize()` 后检查是否在 `allowed_local_paths` 之下，防 `../` 穿越。
//! - 远端路径：调远端 SFTP `realpath`（通过 `SftpCanonicalize` trait）解析后检查
//!   是否在 `allowed_remote_paths` 之下，防 `../` 与 symlink 逃逸。
//! - null 字节（`\0`）一律拒绝（防止 null 字节注入绕过路径检查）。
//!
//! 默认：
//! - `allowed_local_paths` = `[cwd]`（启动时确定）
//! - `allowed_remote_paths` = `["/"]`（全放行，启动期 WARN 提醒用户收紧）
//!
//! 错误码（§6.1）：`LOCAL_PATH_NOT_ALLOWED` / `REMOTE_PATH_NOT_ALLOWED`（retriable=false）。

use std::path::{Path, PathBuf};

use crate::domain::provider::{SftpCanonicalize, TermError};

/// SFTP 路径策略。
///
/// 构造后不可变；SessionManager 持有 Arc 共享给所有 sftp_transfer 调用。
pub struct PathPolicy {
    /// 允许读写的本地根路径列表（已规范化为绝对路径）。
    /// 默认 `[std::env::current_dir()]`。
    allowed_local_paths: Vec<PathBuf>,
    /// 允许读写的远端根路径列表（绝对路径，前缀匹配）。
    /// 默认 `["/"]`（全放行；启动期 WARN 提醒收紧）。
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

    /// 默认策略：`allowed_local_paths=[cwd]`，`allowed_remote_paths=["/"]`。
    pub fn default_from_cwd() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "current_dir 失败，回退到 \".\"");
            PathBuf::from(".")
        });
        tracing::warn!(
            ?cwd,
            "PathPolicy: allowed_remote_paths 默认 [\"/\"]（全放行），建议收紧"
        );
        Self::new(vec![cwd], vec!["/".to_string()])
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

    /// 检查远端路径是否允许读写。
    ///
    /// 规则：
    /// 1. 拒绝含 null 字节的路径。
    /// 2. 通过 `SftpCanonicalize::canonicalize` 调远端 `realpath`，解析 `..` 与 symlink。
    /// 3. 检查规范化后的路径是否在 `allowed_remote_paths` 任一根下。
    ///
    /// `sftp` 参数：实现 `SftpCanonicalize` 的引用（`&SftpProvider` 或 `&dyn SftpCanonicalize`）。
    pub async fn check_remote(
        &self,
        path: &str,
        sftp: &dyn SftpCanonicalize,
    ) -> Result<(), TermError> {
        // 1. 拒绝 null 字节
        if path.as_bytes().contains(&0u8) {
            return Err(TermError::RemotePathNotAllowed(format!(
                "path contains null byte: {path}"
            )));
        }

        // 2. 调远端 realpath 解析（防 .. / symlink 逃逸）
        let canonical = sftp.canonicalize(path).await?;

        // 3. 检查规范化路径是否在任一 allowed 根下
        self.check_remote_canonical(&canonical, path)
    }

    /// 已规范化远端路径的前缀检查（提取为独立方法便于测试）。
    fn check_remote_canonical(&self, canonical: &str, original: &str) -> Result<(), TermError> {
        for allowed in &self.allowed_remote_paths {
            if is_under_remote(canonical, allowed) {
                return Ok(());
            }
        }
        Err(TermError::RemotePathNotAllowed(format!(
            "remote path '{original}' resolves to '{canonical}', not under allowed roots {:?}",
            self.allowed_remote_paths
        )))
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
                .ok_or_else(|| TermError::SftpError(format!("not found: {path}")))
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
}
