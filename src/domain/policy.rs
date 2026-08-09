//! Policy trait —— 危险动作拦截接口（PLAN.md §8）
//!
//! ```text
//! trait Policy {
//!     fn authorize(&self, action: &Action) -> Decision;
//!     fn name(&self) -> &'static str;
//! }
//!
//! enum Decision { Allow, Confirm, Deny }
//! ```
//!
//! 第一版实现 `DefaultPolicy`（Phase 2，application 层），未来加 `RulePolicy`。
//! Policy 在 Application 层拦截，不侵入 SSH / infrastructure 层。
//!
//! 三种判决语义：
//! - `Allow`：放行
//! - `Confirm`：需 Agent 向用户确认（Phase 2 MVP：等同 Deny，返回
//!   `TermError::PolicyNeedsConfirm`；Phase 6 实现 HITL UI 后真正交互确认）
//! - `Deny`：拒绝，返回 `TermError::PolicyDenied`

use crate::domain::provider::TransferDirection;

// ───────────────────────────────────────────────────────────────────────────
// Action —— Policy 判断的输入动作
// ───────────────────────────────────────────────────────────────────────────

/// Policy 判断的输入动作（PLAN.md §8）。
///
/// 每个变体对应一类 Application 层操作；Policy 据此决定 Allow / Confirm / Deny。
/// 字段携带最小必要信息（session_id / 数据 / 路径等）供 Policy 判断与日志记录。
#[derive(Debug, Clone)]
pub enum Action {
    /// 发送输入到 PTY（send_input 工具）。
    ///
    /// `data` 已由原始字节按 UTF-8 lossy 转换为字符串——Policy 只做命令文本检查，
    /// 非 UTF-8 字节不参与匹配（best-effort）。
    SendInput {
        /// 目标 session id（日志用，不参与判决）
        session_id: String,
        /// 输入文本（多行输入按行扫描，任一行命中 blocklist 则整条 Deny）
        data: String,
    },

    /// SFTP 文件传输（sftp_transfer 工具）。
    SftpTransfer {
        /// 传输方向：upload（本地→远端）/ download（远端→本地）
        direction: TransferDirection,
        /// 本地路径
        local: String,
        /// 远端路径
        remote: String,
    },

    /// SFTP 删除（未来 sftp_remove 工具）。
    SftpRemove {
        /// 远端路径
        remote: String,
        /// 是否递归删除目录
        recursive: bool,
    },

    /// SFTP 改权限（未来 sftp_chmod 工具）。
    SftpChmod {
        /// 远端路径
        remote: String,
        /// 权限模式（如 "755" / "u+rwx"）
        mode: String,
    },
}

impl Action {
    /// 便捷构造：SendInput 动作。
    pub fn send_input(session_id: impl Into<String>, data: impl Into<String>) -> Self {
        Self::SendInput {
            session_id: session_id.into(),
            data: data.into(),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Decision —— 判决
// ───────────────────────────────────────────────────────────────────────────

/// Policy 判决结果（PLAN.md §8）。
///
/// Phase 2 MVP 约束（PLAN.md §8 关键约束）：
/// - `Confirm` 等同 `Deny`（无 HITL UI），但错误码区分（`POLICY_NEEDS_CONFIRM`
///   vs `POLICY_DENIED`）供 Agent 区分处理——Agent 应提示用户手动执行。
/// - Phase 6 实现 HITL UI 后，`Confirm` 才真正交互确认。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 放行
    Allow,
    /// 需向用户确认（Phase 2：等同 Deny；Phase 6：HITL 交互）
    Confirm,
    /// 拒绝
    Deny,
}

impl Decision {
    /// 是否终态拒绝（Deny 或 Phase 2 的 Confirm）。
    ///
    /// Phase 2 MVP：`Confirm` 也视为拒绝（无 HITL UI）。
    /// Application 层据此返回对应 TermError。
    pub fn is_blocked_in_phase2(self) -> bool {
        matches!(self, Self::Deny | Self::Confirm)
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Policy trait
// ───────────────────────────────────────────────────────────────────────────

/// 危险动作拦截接口（PLAN.md §8）。
///
/// 第一版简单分类，不做企业级 RBAC/ABAC/OPA。实现方：
/// - `DefaultPolicy`（Phase 2，application 层）：硬编码 blocklist + confirm 列表
/// - `RulePolicy`（未来）：可配置规则
///
/// `Send + Sync`：让 `PolicyManager` 可跨线程持有多个 `Box<dyn Policy>`。
pub trait Policy: Send + Sync {
    /// 判定动作是否允许。
    ///
    /// 实现应保持纯函数语义（无副作用），日志由调用方（PolicyManager）统一记录。
    fn authorize(&self, action: &Action) -> Decision;

    /// Policy 名称（日志/调试用，如 "DefaultPolicy" / "RulePolicy"）。
    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_send_input_constructor() {
        let a = Action::send_input("sess_1", "ls -la\n");
        match a {
            Action::SendInput { session_id, data } => {
                assert_eq!(session_id, "sess_1");
                assert_eq!(data, "ls -la\n");
            }
            _ => panic!("应为 SendInput 变体"),
        }
    }

    #[test]
    fn decision_is_blocked_in_phase2() {
        // Phase 2 MVP：Allow 不阻断，Deny 与 Confirm 都阻断（无 HITL UI）
        assert!(!Decision::Allow.is_blocked_in_phase2());
        assert!(Decision::Deny.is_blocked_in_phase2());
        assert!(Decision::Confirm.is_blocked_in_phase2());
    }

    #[test]
    fn decision_equality() {
        assert_eq!(Decision::Allow, Decision::Allow);
        assert_ne!(Decision::Allow, Decision::Deny);
        assert_ne!(Decision::Confirm, Decision::Deny);
    }
}
