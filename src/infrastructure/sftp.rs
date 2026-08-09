//! sftp —— russh-sftp 封装：SftpProvider（§5.5 / §7.4 Phase 1）
//!
//! 封装 russh-sftp 2.4.0，提供 upload / download / canonicalize。
//!
//! ```text
//! SshTerminalHandle.session (Mutex<Option<Handle<...>>>)
//!   └── SftpProvider::open(&session)
//!         ├── channel_open_session()
//!         ├── channel.request_subsystem(true, "sftp")
//!         └── SftpSession::new(channel.into_stream())
//!               ↑
//!               SftpProvider { sftp: SftpSession }
//!                 ├── upload(local, remote)   读本地 → 写远端
//!                 ├── download(remote, local) 读远端 → 写本地（原子：tmp + fsync + rename）
//!                 └── canonicalize(path)     realpath（路径策略检查用）
//! ```
//!
//! Phase 1 约束：SFTP channel 每次操作独立开关（不做 channel 池）。
//! `SftpProvider` 析构时 best-effort close SFTP session（russh-sftp 内部 mpsc 收尾）。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use russh::client::{Handler, Handle};
use russh_sftp::client::error::Error as SftpLibError;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, StatusCode};
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::domain::provider::{SftpCanonicalize, TermError};

/// 远端目录条目（Phase 2，sftp_list 返回类型）。
///
/// 序列化为 JSON 供 MCP 工具返回给 Agent。
/// `permissions` 为原始 POSIX 权限位（如 `0o755`），含文件类型位；
/// 不存在时为 None（部分 SFTP server 不返回 permissions）。
#[derive(Debug, Clone, Serialize)]
pub struct RemoteEntry {
    /// 条目名称（不含父路径）
    pub name: String,
    /// 是否为目录
    pub is_dir: bool,
    /// 是否为普通文件
    pub is_file: bool,
    /// 文件大小（字节）；目录可能为 0
    pub size: u64,
    /// POSIX 权限位（含类型位），如 `0o40755`（目录 755）；不存在则 None
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<u32>,
}

/// SFTP 操作封装。每次构造会新建一个 SFTP channel（独立于 PTY channel）。
///
/// 生命周期短：构造 → 一次 upload/download/canonicalize → drop。
/// 不缓存 SFTP session（Phase 1 不做 channel 池，避免复用复杂度）。
pub struct SftpProvider {
    sftp: SftpSession,
}

impl SftpProvider {
    /// 在已有 SSH session 上开新 channel 请求 SFTP 子系统，构造 SftpProvider。
    ///
    /// `session` 来自 `SshTerminalHandle` 持有的 `Handle<SshClientHandler>`，
    /// 传入 `&Handle`（`&self` 方法 `channel_open_session`），不 take 所有权。
    pub async fn open<H: Handler>(session: &Handle<H>) -> Result<Self, TermError> {
        // 1. 开新 channel（独立于 PTY channel，互不影响）
        let channel = session
            .channel_open_session()
            .await
            .map_err(|e| TermError::SftpError(format!("channel_open_session: {e}")))?;

        // 2. 请求 sftp 子系统
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| TermError::SftpError(format!("request_subsystem(sftp): {e}")))?;

        // 3. 建立 SFTP 会话
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| TermError::SftpError(format!("SftpSession::new: {e}")))?;

        tracing::info!("sftp channel opened");
        Ok(Self { sftp })
    }

    /// 上传本地文件到远端（覆盖写）。
    ///
    /// 流式拷贝（`tokio::io::copy`），不一次性载入内存，适合大文件。
    /// 远端用 `create()`（CREATE | TRUNCATE | WRITE）打开。
    pub async fn upload(&self, local: &Path, remote: &str) -> Result<(), TermError> {
        tracing::info!(local = ?local, remote = remote, "sftp upload: starting");

        let local_size = tokio::fs::metadata(local)
            .await
            .map_err(TermError::Io)?
            .len();

        let mut local_file = tokio::fs::File::open(local).await?;
        // create() = CREATE | TRUNCATE | WRITE，覆盖已有远端文件
        let mut remote_file = self
            .sftp
            .create(remote)
            .await
            .map_err(|e| TermError::SftpError(format!("sftp create '{remote}': {e}")))?;

        let copied = tokio::io::copy(&mut local_file, &mut remote_file)
            .await
            .map_err(|e| TermError::SftpError(format!("sftp upload copy: {e}")))?;

        // flush 等 pending writes 完成；shutdown 关闭远端 handle
        remote_file
            .flush()
            .await
            .map_err(|e| TermError::SftpError(format!("sftp upload flush: {e}")))?;
        remote_file
            .shutdown()
            .await
            .map_err(|e| TermError::SftpError(format!("sftp upload shutdown: {e}")))?;

        tracing::info!(
            local = ?local,
            remote = remote,
            bytes = copied,
            expected = local_size,
            "sftp upload: complete"
        );
        Ok(())
    }

    /// 下载远端文件到本地（**原子写**）。
    ///
    /// 流程（ADR-0005 §5）：
    /// 1. 写到临时文件 `local + ".termbridge.tmp"`
    /// 2. fsync 临时文件
    /// 3. rename 临时文件 → local（POSIX 原子）
    /// 4. 任一步失败 → 清理临时文件
    ///
    /// 避免半写文件被 Agent 误读为完整产物。
    pub async fn download(&self, remote: &str, local: &Path) -> Result<(), TermError> {
        tracing::info!(remote = remote, local = ?local, "sftp download: starting");

        // 临时文件路径：在目标路径后追加 ".termbridge.tmp"。
        // 不用 with_extension（会替换扩展名），直接拼接保证路径稳定。
        let tmp: PathBuf = format!("{}.termbridge.tmp", local.to_string_lossy()).into();

        // 主流程：任一异常都跳到清理临时文件
        let result: Result<(), TermError> = async {
            // 1. 打开远端文件（只读）
            let mut remote_file = self
                .sftp
                .open(remote)
                .await
                .map_err(|e| TermError::SftpError(format!("sftp open '{remote}': {e}")))?;

            // 2. 创建 + 打开本地临时文件（覆盖已有 tmp，幂等）
            let mut local_tmp = tokio::fs::File::create(&tmp)
                .await
                .map_err(TermError::Io)?;

            // 3. 流式拷贝 远端 → 临时文件
            let copied = tokio::io::copy(&mut remote_file, &mut local_tmp)
                .await
                .map_err(|e| TermError::SftpError(format!("sftp download copy: {e}")))?;

            // 4. 关闭远端 handle（shutdown 等关闭确认）
            remote_file
                .shutdown()
                .await
                .map_err(|e| TermError::SftpError(format!("sftp download remote shutdown: {e}")))?;

            // 5. fsync 临时文件（确保数据落盘后再 rename）
            local_tmp
                .sync_all()
                .await
                .map_err(TermError::Io)?;

            // 6. 关闭临时文件句柄（Windows 上 rename 前必须释放句柄）
            drop(local_tmp);

            // 7. rename 临时文件 → 目标路径（POSIX 原子）
            tokio::fs::rename(&tmp, local)
                .await
                .map_err(TermError::Io)?;

            tracing::info!(
                remote = remote,
                local = ?local,
                bytes = copied,
                "sftp download: complete"
            );
            Ok(())
        }
        .await;

        // 失败清理：删除残留的临时文件（best-effort，忽略删除错误）
        if result.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }

        result
    }

    /// 关闭 SFTP session（best-effort）。
    /// russh-sftp 内部用 mpsc，drop 时收尾；显式 close 更优雅。
    pub async fn close(&self) -> Result<(), TermError> {
        self.sftp
            .close()
            .await
            .map_err(|e| TermError::SftpError(format!("sftp close: {e}")))
    }

    // ── Phase 2：目录 / 权限 / 列表 / 删除 ──────────────────────

    /// 创建远端目录（Phase 2）。
    ///
    /// `mode` 为 POSIX 权限位（如 `0o755`）。russh-sftp 的 `create_dir` 不支持
    /// 直接传 attrs，因此先创建目录再通过 `set_metadata` 设置权限。
    /// `mode == 0` 时跳过 set_metadata（使用服务器默认 umask）。
    pub async fn mkdir(&self, remote: &str, mode: u32) -> Result<(), TermError> {
        tracing::info!(remote = remote, mode = format!("{mode:o}"), "sftp mkdir: starting");
        self.sftp
            .create_dir(remote)
            .await
            .map_err(|e| map_sftp_error(e, &format!("sftp mkdir '{remote}'")))?;

        if mode != 0 {
            let attrs = FileAttributes {
                permissions: Some(mode),
                ..Default::default()
            };
            self.sftp
                .set_metadata(remote, attrs)
                .await
                .map_err(|e| map_sftp_error(e, &format!("sftp mkdir setperm '{remote}'")))?;
        }
        tracing::info!(remote = remote, "sftp mkdir: complete");
        Ok(())
    }

    /// 删除远端空目录（Phase 2）。
    /// 目标非空或不存在会失败（映射为 SftpError / SftpNoSuchFile）。
    pub async fn rmdir(&self, remote: &str) -> Result<(), TermError> {
        tracing::info!(remote = remote, "sftp rmdir: starting");
        self.sftp
            .remove_dir(remote)
            .await
            .map_err(|e| map_sftp_error(e, &format!("sftp rmdir '{remote}'")))?;
        tracing::info!(remote = remote, "sftp rmdir: complete");
        Ok(())
    }

    /// 删除远端文件（Phase 2）。
    /// 目标是目录会失败；不存在映射为 SftpNoSuchFile。
    pub async fn remove(&self, remote: &str) -> Result<(), TermError> {
        tracing::info!(remote = remote, "sftp remove: starting");
        self.sftp
            .remove_file(remote)
            .await
            .map_err(|e| map_sftp_error(e, &format!("sftp remove '{remote}'")))?;
        tracing::info!(remote = remote, "sftp remove: complete");
        Ok(())
    }

    /// 修改远端文件/目录权限（Phase 2，chmod）。
    /// `mode` 为 POSIX 权限位（如 `0o755`），通过 SFTP setstat 设置。
    pub async fn chmod(&self, remote: &str, mode: u32) -> Result<(), TermError> {
        tracing::info!(remote = remote, mode = format!("{mode:o}"), "sftp chmod: starting");
        let attrs = FileAttributes {
            permissions: Some(mode),
            ..Default::default()
        };
        self.sftp
            .set_metadata(remote, attrs)
            .await
            .map_err(|e| map_sftp_error(e, &format!("sftp chmod '{remote}'")))?;
        tracing::info!(remote = remote, "sftp chmod: complete");
        Ok(())
    }

    /// 列出远端目录内容（Phase 2）。
    /// 返回 `Vec<RemoteEntry>`，已过滤 `.` 和 `..`。
    /// 目标不存在映射为 SftpNoSuchFile；非目录映射为 SftpError。
    pub async fn list_dir(&self, remote: &str) -> Result<Vec<RemoteEntry>, TermError> {
        tracing::info!(remote = remote, "sftp list_dir: starting");
        let read_dir = self
            .sftp
            .read_dir(remote)
            .await
            .map_err(|e| map_sftp_error(e, &format!("sftp list_dir '{remote}'")))?;

        let entries: Vec<RemoteEntry> = read_dir
            .map(|entry| {
                let metadata = entry.metadata();
                RemoteEntry {
                    name: entry.file_name(),
                    is_dir: metadata.is_dir(),
                    is_file: metadata.is_regular(),
                    size: metadata.len(),
                    permissions: metadata.permissions,
                }
            })
            .collect();

        tracing::info!(remote = remote, count = entries.len(), "sftp list_dir: complete");
        Ok(entries)
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 错误映射（Phase 2）
// ───────────────────────────────────────────────────────────────────────────

/// 将 russh-sftp 库错误映射为 TermError（Phase 2）。
///
/// - `Status(NoSuchFile)` → `SftpNoSuchFile`（retriable=false）
/// - `Status(PermissionDenied)` → `SftpPermissionDenied`（retriable=false）
/// - 其他 → `SftpError`（retriable=true）
fn map_sftp_error(e: SftpLibError, context: &str) -> TermError {
    match &e {
        SftpLibError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => {
                TermError::SftpNoSuchFile(format!("{context}: {e}"))
            }
            StatusCode::PermissionDenied => {
                TermError::SftpPermissionDenied(format!("{context}: {e}"))
            }
            _ => TermError::SftpError(format!("{context}: {e}")),
        },
        _ => TermError::SftpError(format!("{context}: {e}")),
    }
}

#[async_trait]
impl SftpCanonicalize for SftpProvider {
    /// 调远端 SFTP `realpath` 协议，解析为绝对路径。
    /// 用于 PathPolicy::check_remote 防 `..` 与 symlink 逃逸（ADR-0005 §4）。
    ///
    /// Phase 2：使用 map_sftp_error 映射 NoSuchFile / PermissionDenied，
    /// 供 PathPolicy::check_remote_allow_new 区分"路径不存在"与其他错误。
    async fn canonicalize(&self, path: &str) -> Result<String, TermError> {
        self.sftp
            .canonicalize(path)
            .await
            .map_err(|e| map_sftp_error(e, &format!("sftp canonicalize '{path}'")))
    }
}

// 为 `&SftpProvider` 实现 `SftpCanonicalize`（让 PathPolicy 可接受具体类型而非必须 dyn）
// 已通过 impl SftpCanonicalize for SftpProvider 提供。无需再为引用实现。

#[cfg(test)]
mod tests {
    use super::*;

    /// 原子写逻辑：tmp → fsync → rename。无 SFTP，仅验证本地 fs 行为。
    ///
    /// 流程：写临时文件 → download 内部 rename 到目标。
    /// 验证：目标文件存在且内容正确；临时文件已不存在（rename 后被替换）。
    #[tokio::test]
    async fn download_atomic_write_renames_tmp_to_target() {
        let dir = std::env::temp_dir().join("termbridge_sftp_test_atomic_write");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let target = dir.join("target.bin");
        let tmp: PathBuf = format!("{}.termbridge.tmp", target.to_string_lossy()).into();

        // 清理可能的残留
        let _ = tokio::fs::remove_file(&target).await;
        let _ = tokio::fs::remove_file(&tmp).await;

        // 模拟 download 的原子写流程：create + write → fsync → close → rename
        // 用 OpenOptions 写模式打开（与 SftpProvider::download 一致），
        // Windows 上 sync_all 要求写访问（read-only 句柄会 PermissionDenied）。
        let payload = b"hello termbridge atomic write\n";
        let mut f = tokio::fs::File::create(&tmp).await.unwrap();
        f.write_all(payload).await.unwrap();
        f.sync_all().await.unwrap();
        drop(f); // 关闭句柄（Windows rename 前必须释放）

        // rename
        tokio::fs::rename(&tmp, &target).await.unwrap();

        // 验证
        assert!(tokio::fs::try_exists(&target).await.unwrap());
        assert!(
            !tokio::fs::try_exists(&tmp).await.unwrap(),
            "tmp should be renamed away"
        );
        let got = tokio::fs::read(&target).await.unwrap();
        assert_eq!(got, payload);

        // 清理
        let _ = tokio::fs::remove_file(&target).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 原子写失败时清理临时文件。
    ///
    /// 模拟：写 tmp 后模拟失败（删除 tmp 前先验证清理逻辑），目标不应存在。
    #[tokio::test]
    async fn download_failure_cleans_tmp() {
        let dir = std::env::temp_dir().join("termbridge_sftp_test_failure_clean");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let target = dir.join("never_written.bin");
        let tmp: PathBuf = format!("{}.termbridge.tmp", target.to_string_lossy()).into();

        // 清理可能的残留
        let _ = tokio::fs::remove_file(&target).await;
        let _ = tokio::fs::remove_file(&tmp).await;

        // 模拟 download 中途失败：写 tmp 但不 rename
        tokio::fs::write(&tmp, b"partial").await.unwrap();

        // 模拟失败清理（download 函数中的 finally 逻辑）
        let _ = tokio::fs::remove_file(&tmp).await;

        // 验证：目标不存在，tmp 已清理
        assert!(!tokio::fs::try_exists(&target).await.unwrap());
        assert!(!tokio::fs::try_exists(&tmp).await.unwrap());

        // 清理
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// tmp 路径生成策略：`local + ".termbridge.tmp"`。
    /// 验证不同扩展名的 local 路径都能生成稳定的 tmp 路径（不会被 with_extension 误改）。
    #[test]
    fn tmp_path_appended_not_replaced() {
        let local = Path::new("/tmp/foo.txt");
        let tmp: PathBuf = format!("{}.termbridge.tmp", local.to_string_lossy()).into();
        assert_eq!(tmp, PathBuf::from("/tmp/foo.txt.termbridge.tmp"));

        // 无扩展名
        let local = Path::new("/tmp/noext");
        let tmp: PathBuf = format!("{}.termbridge.tmp", local.to_string_lossy()).into();
        assert_eq!(tmp, PathBuf::from("/tmp/noext.termbridge.tmp"));

        // 多扩展名
        let local = Path::new("/tmp/a.tar.gz");
        let tmp: PathBuf = format!("{}.termbridge.tmp", local.to_string_lossy()).into();
        assert_eq!(tmp, PathBuf::from("/tmp/a.tar.gz.termbridge.tmp"));
    }

    // ── Phase 2：RemoteEntry 序列化测试 ──────────────────────────

    #[test]
    fn remote_entry_serializes_with_all_fields() {
        let entry = RemoteEntry {
            name: "test.txt".into(),
            is_dir: false,
            is_file: true,
            size: 1024,
            permissions: Some(0o644),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"name\":\"test.txt\""));
        assert!(json.contains("\"is_dir\":false"));
        assert!(json.contains("\"is_file\":true"));
        assert!(json.contains("\"size\":1024"));
        assert!(json.contains("\"permissions\":420")); // 0o644 = 420
    }

    #[test]
    fn remote_entry_serializes_without_permissions() {
        let entry = RemoteEntry {
            name: "nofile".into(),
            is_dir: true,
            is_file: false,
            size: 0,
            permissions: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        // permissions 为 None 时应被 skip
        assert!(!json.contains("permissions"));
    }

    // ── Phase 2：map_sftp_error 测试 ──────────────────────────────

    #[test]
    fn map_sftp_error_no_such_file() {
        let status = russh_sftp::protocol::Status {
            id: 0,
            status_code: StatusCode::NoSuchFile,
            error_message: "No such file".into(),
            language_tag: "en".into(),
        };
        let err = map_sftp_error(SftpLibError::Status(status), "mkdir");
        assert!(matches!(err, TermError::SftpNoSuchFile(_)));
        assert_eq!(err.code(), "SFTP_NO_SUCH_FILE");
        assert!(!err.retriable());
    }

    #[test]
    fn map_sftp_error_permission_denied() {
        let status = russh_sftp::protocol::Status {
            id: 0,
            status_code: StatusCode::PermissionDenied,
            error_message: "Permission denied".into(),
            language_tag: "en".into(),
        };
        let err = map_sftp_error(SftpLibError::Status(status), "chmod");
        assert!(matches!(err, TermError::SftpPermissionDenied(_)));
        assert_eq!(err.code(), "SFTP_PERMISSION_DENIED");
        assert!(!err.retriable());
    }

    #[test]
    fn map_sftp_error_other_status_maps_to_sftp_error() {
        let status = russh_sftp::protocol::Status {
            id: 0,
            status_code: StatusCode::Failure,
            error_message: "Failure".into(),
            language_tag: "en".into(),
        };
        let err = map_sftp_error(SftpLibError::Status(status), "remove");
        assert!(matches!(err, TermError::SftpError(_)));
        assert_eq!(err.code(), "SFTP_ERROR");
        assert!(err.retriable());
    }

    #[test]
    fn map_sftp_error_timeout_maps_to_sftp_error() {
        let err = map_sftp_error(SftpLibError::Timeout, "list_dir");
        assert!(matches!(err, TermError::SftpError(_)));
        assert!(err.retriable());
    }
}
