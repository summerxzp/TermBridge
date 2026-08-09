//! PolicyManager + DefaultPolicy —— Application 层危险动作拦截（PLAN.md §8 / Phase 2）
//!
//! 设计：
//! - `DefaultPolicy`：硬编码 blocklist（Deny）+ confirm 列表（Confirm）+ 默认 Allow
//! - `PolicyManager`：管理多个 Policy 链，按顺序调用，任一 Deny 则 Deny
//!
//! 关键约束（PLAN.md §8）：
//! - Policy 在 Application 层拦截，不侵入 SSH / infrastructure 层
//! - blocklist 是 best-effort（命令可变形成绕过），日志 WARN 提示非绝对安全
//! - Phase 2 的 Confirm 等同 Deny（无 HITL UI），但错误码区分供 Agent 处理
//!
//! 正则编译：用 `std::sync::OnceLock` 进程级编译一次复用（借鉴 infrastructure/redact.rs）。

use std::sync::OnceLock;

use regex::Regex;

use crate::domain::policy::{Action, Decision, Policy};

// ───────────────────────────────────────────────────────────────────────────
// blocklist / confirm 正则（进程级编译一次）
// ───────────────────────────────────────────────────────────────────────────

/// blocklist 正则：命中即 Deny（PLAN.md §8，Phase 2 硬编码）。
///
/// 顺序：先 blocklist（Deny），后 confirm（Confirm）。
/// 任一行命中 blocklist → 整条 Deny。
static BLOCKLIST_RES: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();

/// confirm 列表正则：命中即 Confirm（PLAN.md §8，Phase 2 硬编码）。
static CONFIRM_RES: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();

/// 获取 blocklist 正则列表（懒编译）。
fn blocklist() -> &'static [(&'static str, Regex)] {
    BLOCKLIST_RES.get_or_init(|| {
        vec![
            // rm -rf / —— 递归删根（覆盖 -rf / -fr / -rvf 等组合，目标为根 /）
            // 匹配 / 后跟空白或行尾，避免误杀 /home 等
            (
                "rm -rf / (递归删根)",
                Regex::new(r"rm\s+-[a-zA-Z]*r[a-zA-Z]*f[a-zA-Z]*\s+/(\s|$)")
                    .expect("blocklist 正则 rm-rf-root 编译失败"),
            ),
            (
                "rm -fr / (递归删根)",
                Regex::new(r"rm\s+-[a-zA-Z]*f[a-zA-Z]*r[a-zA-Z]*\s+/(\s|$)")
                    .expect("blocklist 正则 rm-fr-root 编译失败"),
            ),
            // mkfs —— 格式化文件系统
            (
                "mkfs (格式化)",
                Regex::new(r"\bmkfs(\.\w+)?\b")
                    .expect("blocklist 正则 mkfs 编译失败"),
            ),
            // dd if=...of=/dev/... —— 写块设备
            (
                "dd of=/dev/ (写设备)",
                Regex::new(r"\bdd\b.*\bof=/dev/")
                    .expect("blocklist 正则 dd-of-dev 编译失败"),
            ),
            // fork bomb :(){:|:&};: —— 检测函数定义特征 :(){
            (
                "fork bomb :(){",
                Regex::new(r":\s*\(\s*\)\s*\{")
                    .expect("blocklist 正则 fork-bomb 编译失败"),
            ),
            // shutdown / reboot / halt / poweroff / init 0 —— 关机重启
            (
                "shutdown/reboot/halt/poweroff (关机重启)",
                Regex::new(r"\b(shutdown|reboot|halt|poweroff|init\s+0)\b")
                    .expect("blocklist 正则 shutdown 编译失败"),
            ),
            // chmod -R 777 / —— 全局权限放开
            (
                "chmod -R 777 / (全局权限)",
                Regex::new(r"chmod\s+-R\s+777\s+/(\s|$)")
                    .expect("blocklist 正则 chmod-777 编译失败"),
            ),
            // > /dev/sdX / /dev/nvme... —— 重定向写块设备
            (
                "> /dev/ (重定向写设备)",
                Regex::new(r">\s*/dev/(sd|nvme|hd|vd|xvd|disk)")
                    .expect("blocklist 正则 redirect-dev 编译失败"),
            ),
            // :(){:|:&};: fork bomb 完整形式 —— 也检测 :|:& 核心片段
            (
                "fork bomb :|:&",
                Regex::new(r":\s*\|\s*:\s*&")
                    .expect("blocklist 正则 fork-bomb-core 编译失败"),
            ),
        ]
    })
}

/// 获取 confirm 正则列表（懒编译）。
fn confirm_list() -> &'static [(&'static str, Regex)] {
    CONFIRM_RES.get_or_init(|| {
        vec![
            // rm -rf <非根> —— 递归删除（非根目录，根目录已被 blocklist 拦截）
            (
                "rm -rf (递归删除)",
                Regex::new(r"rm\s+-[a-zA-Z]*r[a-zA-Z]*f[a-zA-Z]*\s+\S")
                    .expect("confirm 正则 rm-rf 编译失败"),
            ),
            (
                "rm -fr (递归删除)",
                Regex::new(r"rm\s+-[a-zA-Z]*f[a-zA-Z]*r[a-zA-Z]*\s+\S")
                    .expect("confirm 正则 rm-fr 编译失败"),
            ),
            // sudo —— 提权
            (
                "sudo (提权)",
                Regex::new(r"\bsudo\b")
                    .expect("confirm 正则 sudo 编译失败"),
            ),
            // kill -9 —— 强杀进程
            (
                "kill -9 (强杀)",
                Regex::new(r"\bkill\s+-9\b")
                    .expect("confirm 正则 kill-9 编译失败"),
            ),
            // kill -KILL —— 强杀进程（信号名形式）
            (
                "kill -KILL (强杀)",
                Regex::new(r"\bkill\s+-KILL\b")
                    .expect("confirm 正则 kill-KILL 编译失败"),
            ),
            // iptables —— 防火墙修改
            (
                "iptables (防火墙修改)",
                Regex::new(r"\biptables\b")
                    .expect("confirm 正则 iptables 编译失败"),
            ),
            // crontab -r —— 删除 cron 任务
            (
                "crontab -r (删 cron)",
                Regex::new(r"\bcrontab\s+-r\b")
                    .expect("confirm 正则 crontab-r 编译失败"),
            ),
        ]
    })
}

// ───────────────────────────────────────────────────────────────────────────
// DefaultPolicy
// ───────────────────────────────────────────────────────────────────────────

/// 默认策略（PLAN.md §8，Phase 2）。
///
/// 规则：
/// - `Action::SendInput`：多行输入按行扫描，任一行命中 blocklist → Deny；
///   否则任一行命中 confirm → Confirm；都不命中 → Allow。
/// - `Action::SftpTransfer`：upload 到 /dev/、/etc/ 等敏感路径 → Confirm；
///   download 一般 Allow（路径策略由 PathPolicy 单独管）。
/// - `Action::SftpRemove`：recursive 删除 → Deny；非递归 → Confirm。
/// - `Action::SftpChmod`：777 → Confirm；其他 Allow。
///
/// blocklist 是 best-effort：命令可变形成绕过（如 base64 编码、变量拼接），
/// 日志 WARN 提示非绝对安全。真正的安全应靠最小权限原则 + 审计。
pub struct DefaultPolicy;

impl DefaultPolicy {
    /// 创建默认策略实例。
    pub fn new() -> Self {
        // 启动时 WARN 提示：blocklist 是 best-effort，非绝对安全
        tracing::warn!(
            "DefaultPolicy: blocklist 为 best-effort（命令可变形成绕过），\
             非绝对安全；生产环境应配合最小权限 + 审计"
        );
        Self
    }

    /// 对单行命令文本做 blocklist + confirm 检查。
    ///
    /// 返回：Deny / Confirm / None（未命中任何规则）。
    fn check_line(line: &str) -> Option<Decision> {
        // 1. blocklist 优先：任一命中 → Deny
        for (desc, re) in blocklist() {
            if re.is_match(line) {
                tracing::warn!(
                    rule = %desc,
                    line = %line,
                    "DefaultPolicy: 命中 blocklist，Deny"
                );
                return Some(Decision::Deny);
            }
        }
        // 2. confirm：任一命中 → Confirm
        for (desc, re) in confirm_list() {
            if re.is_match(line) {
                tracing::info!(
                    rule = %desc,
                    line = %line,
                    "DefaultPolicy: 命中 confirm 列表，Confirm"
                );
                return Some(Decision::Confirm);
            }
        }
        None
    }

    /// 对多行输入按行扫描。
    ///
    /// 任一行命中 blocklist → Deny；否则任一行命中 confirm → Confirm；都不命中 → Allow。
    ///
    /// 优先级与 PolicyManager 一致：Deny 立即短路返回，Confirm 不短路（继续扫描后续行，
    /// 以防后面有 blocklist 命中）。
    fn check_send_input(data: &str) -> Decision {
        // 按行扫描（含 \r\n / \n）；空行跳过
        let mut result = Decision::Allow;
        for line in data.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(d) = Self::check_line(trimmed) {
                match d {
                    Decision::Deny => return Decision::Deny,
                    Decision::Confirm => result = Decision::Confirm,
                    Decision::Allow => {}
                }
            }
        }
        result
    }

    /// 对 SFTP 传输动作检查。
    fn check_sftp_transfer(direction: &crate::domain::provider::TransferDirection, remote: &str) -> Decision {
        use crate::domain::provider::TransferDirection;
        // upload 到敏感路径（/dev/、/etc/、/boot/、/sys/）→ Confirm
        if matches!(direction, TransferDirection::Upload) {
            let sensitive = ["/dev/", "/etc/", "/boot/", "/sys/", "/proc/"];
            if sensitive.iter().any(|p| remote.starts_with(p)) {
                return Decision::Confirm;
            }
        }
        Decision::Allow
    }

    /// 对 SFTP 删除动作检查。
    fn check_sftp_remove(remote: &str, recursive: bool) -> Decision {
        // 递归删除根或 /etc / /usr 等系统目录 → Deny
        if recursive {
            let critical = ["/", "/etc", "/usr", "/var", "/boot", "/sys", "/proc", "/lib", "/bin", "/sbin"];
            if critical.iter().any(|p| remote == *p || remote.starts_with(&format!("{p}/"))) {
                return Decision::Deny;
            }
            // 其他递归删除 → Confirm
            return Decision::Confirm;
        }
        // 非递归删除 → Confirm（删除本身有风险）
        Decision::Confirm
    }

    /// 对 SFTP chmod 动作检查。
    fn check_sftp_chmod(remote: &str, mode: &str) -> Decision {
        // chmod 777 系统目录 → Confirm
        if mode == "777" || mode.contains("777") {
            let sensitive = ["/etc", "/var", "/usr", "/boot", "/sys", "/bin", "/sbin", "/lib"];
            if sensitive.iter().any(|p| remote == *p || remote.starts_with(&format!("{p}/"))) {
                return Decision::Confirm;
            }
        }
        Decision::Allow
    }
}

impl Default for DefaultPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl Policy for DefaultPolicy {
    fn authorize(&self, action: &Action) -> Decision {
        match action {
            Action::SendInput { data, .. } => Self::check_send_input(data),
            Action::SftpTransfer { direction, remote, .. } => {
                Self::check_sftp_transfer(direction, remote)
            }
            Action::SftpRemove { remote, recursive } => {
                Self::check_sftp_remove(remote, *recursive)
            }
            Action::SftpChmod { remote, mode } => Self::check_sftp_chmod(remote, mode),
        }
    }

    fn name(&self) -> &'static str {
        "DefaultPolicy"
    }
}

// ───────────────────────────────────────────────────────────────────────────
// PolicyManager —— 管理多个 Policy 链
// ───────────────────────────────────────────────────────────────────────────

/// Policy 链管理器（PLAN.md §8）。
///
/// 按顺序调用各 Policy，合并判决：
/// - 任一 Deny → Deny（最严格优先）
/// - 任一 Confirm（无 Deny）→ Confirm
/// - 全 Allow → Allow
///
/// DefaultPolicy 作为默认链中的第一个。未来可加 RulePolicy 等。
pub struct PolicyManager {
    policies: Vec<Box<dyn Policy>>,
}

impl PolicyManager {
    /// 创建空链的 PolicyManager（调用方自行 add Policy）。
    pub fn new() -> Self {
        Self { policies: Vec::new() }
    }

    /// 创建默认链：[DefaultPolicy]。
    pub fn with_default() -> Self {
        let mut mgr = Self::new();
        mgr.add(Box::new(DefaultPolicy::new()));
        mgr
    }

    /// 追加一个 Policy 到链尾。
    pub fn add(&mut self, policy: Box<dyn Policy>) {
        self.policies.push(policy);
    }

    /// 链中 Policy 数量。
    pub fn len(&self) -> usize {
        self.policies.len()
    }

    /// 链是否为空。
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// 按顺序调用各 Policy，合并判决。
    ///
    /// 合并规则（PLAN.md §8）：
    /// - 任一 Deny → Deny
    /// - 任一 Confirm（无 Deny）→ Confirm
    /// - 全 Allow → Allow
    pub fn authorize(&self, action: &Action) -> Decision {
        let mut result = Decision::Allow;
        for p in &self.policies {
            let d = p.authorize(action);
            match d {
                Decision::Deny => {
                    // 任一 Deny 立即返回 Deny（最严格优先）
                    tracing::info!(
                        policy = %p.name(),
                        "PolicyManager: policy 返回 Deny，链式判决 Deny"
                    );
                    return Decision::Deny;
                }
                Decision::Confirm => {
                    // Confirm 记录，继续检查后续 Policy 是否 Deny
                    tracing::info!(
                        policy = %p.name(),
                        "PolicyManager: policy 返回 Confirm，继续检查后续"
                    );
                    result = Decision::Confirm;
                }
                Decision::Allow => {}
            }
        }
        result
    }
}

impl Default for PolicyManager {
    fn default() -> Self {
        Self::with_default()
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 测试
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provider::TransferDirection;

    // ── DefaultPolicy blocklist 测试 ──────────────────────────────────

    #[test]
    fn blocklist_rm_rf_root_denied() {
        let p = DefaultPolicy::new();
        for cmd in [
            "rm -rf /",
            "rm -rf / ",
            "rm -rf /home && rm -rf /",
            "rm -rvf /",
            "rm -fr /",
            "rm -rfv /",
        ] {
            let d = p.authorize(&Action::send_input("s1", cmd));
            assert_eq!(d, Decision::Deny, "应 Deny: {cmd}");
        }
    }

    #[test]
    fn blocklist_mkfs_denied() {
        let p = DefaultPolicy::new();
        for cmd in ["mkfs.ext4 /dev/sda1", "mkfs /dev/sda1", "mkfs.xfs /dev/nvme0n1"] {
            let d = p.authorize(&Action::send_input("s1", cmd));
            assert_eq!(d, Decision::Deny, "应 Deny: {cmd}");
        }
    }

    #[test]
    fn blocklist_dd_to_device_denied() {
        let p = DefaultPolicy::new();
        for cmd in [
            "dd if=/dev/zero of=/dev/sda bs=1M",
            "dd if=image.iso of=/dev/sdb",
        ] {
            let d = p.authorize(&Action::send_input("s1", cmd));
            assert_eq!(d, Decision::Deny, "应 Deny: {cmd}");
        }
    }

    #[test]
    fn blocklist_fork_bomb_denied() {
        let p = DefaultPolicy::new();
        for cmd in [":(){ :|:& };:", ":(){:|:&};:", ": () { : | : & } ; :"] {
            let d = p.authorize(&Action::send_input("s1", cmd));
            assert_eq!(d, Decision::Deny, "应 Deny: {cmd}");
        }
    }

    #[test]
    fn blocklist_shutdown_reboot_denied() {
        let p = DefaultPolicy::new();
        for cmd in ["shutdown -h now", "reboot", "halt", "poweroff", "init 0"] {
            let d = p.authorize(&Action::send_input("s1", cmd));
            assert_eq!(d, Decision::Deny, "应 Deny: {cmd}");
        }
    }

    #[test]
    fn blocklist_chmod_777_root_denied() {
        let p = DefaultPolicy::new();
        for cmd in ["chmod -R 777 /", "chmod -R 777 / "] {
            let d = p.authorize(&Action::send_input("s1", cmd));
            assert_eq!(d, Decision::Deny, "应 Deny: {cmd}");
        }
    }

    #[test]
    fn blocklist_redirect_to_device_denied() {
        let p = DefaultPolicy::new();
        for cmd in [
            "echo foo > /dev/sda",
            "cat /dev/zero > /dev/sdb",
            "dd if=/dev/zero > /dev/nvme0n1",
        ] {
            let d = p.authorize(&Action::send_input("s1", cmd));
            assert_eq!(d, Decision::Deny, "应 Deny: {cmd}");
        }
    }

    // ── DefaultPolicy confirm 测试 ───────────────────────────────────

    #[test]
    fn confirm_sudo_returns_confirm() {
        let p = DefaultPolicy::new();
        for cmd in ["sudo ls", "sudo apt update", "sudo -i"] {
            let d = p.authorize(&Action::send_input("s1", cmd));
            assert_eq!(d, Decision::Confirm, "应 Confirm: {cmd}");
        }
    }

    #[test]
    fn confirm_rm_rf_non_root_returns_confirm() {
        let p = DefaultPolicy::new();
        // rm -rf /tmp/foo 命中 confirm 但不命中 blocklist（/ 后跟 tmp 不是空白/行尾）
        for cmd in ["rm -rf /tmp/foo", "rm -rf ./build", "rm -fr ~/old"] {
            let d = p.authorize(&Action::send_input("s1", cmd));
            assert_eq!(d, Decision::Confirm, "应 Confirm: {cmd}");
        }
    }

    #[test]
    fn confirm_kill_9_returns_confirm() {
        let p = DefaultPolicy::new();
        for cmd in ["kill -9 1234", "kill -KILL 5678"] {
            let d = p.authorize(&Action::send_input("s1", cmd));
            assert_eq!(d, Decision::Confirm, "应 Confirm: {cmd}");
        }
    }

    #[test]
    fn confirm_iptables_returns_confirm() {
        let p = DefaultPolicy::new();
        for cmd in ["iptables -F", "iptables -A INPUT -p tcp --dport 22 -j ACCEPT"] {
            let d = p.authorize(&Action::send_input("s1", cmd));
            assert_eq!(d, Decision::Confirm, "应 Confirm: {cmd}");
        }
    }

    #[test]
    fn confirm_crontab_r_returns_confirm() {
        let p = DefaultPolicy::new();
        let d = p.authorize(&Action::send_input("s1", "crontab -r"));
        assert_eq!(d, Decision::Confirm);
    }

    // ── DefaultPolicy allow 测试 ─────────────────────────────────────

    #[test]
    fn allow_normal_commands() {
        let p = DefaultPolicy::new();
        for cmd in [
            "ls -la",
            "pwd",
            "echo hello",
            "cat /etc/hostname",
            "grep foo bar.txt",
            "python -m http.server",
            "git status",
            "cd /tmp && ls",
            "ps aux",
            "df -h",
        ] {
            let d = p.authorize(&Action::send_input("s1", cmd));
            assert_eq!(d, Decision::Allow, "应 Allow: {cmd}");
        }
    }

    #[test]
    fn allow_empty_input() {
        let p = DefaultPolicy::new();
        let d = p.authorize(&Action::send_input("s1", ""));
        assert_eq!(d, Decision::Allow);
    }

    // ── 多行输入扫描测试 ─────────────────────────────────────────────

    #[test]
    fn multiline_blocklist_any_line_denies_all() {
        let p = DefaultPolicy::new();
        // 第一行普通命令，第二行危险命令 → 整条 Deny
        let input = "ls -la\nrm -rf /\necho done";
        let d = p.authorize(&Action::send_input("s1", input));
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn multiline_confirm_any_line_confirms() {
        let p = DefaultPolicy::new();
        let input = "ls -la\nsudo apt update\necho done";
        let d = p.authorize(&Action::send_input("s1", input));
        assert_eq!(d, Decision::Confirm);
    }

    #[test]
    fn multiline_all_allow_returns_allow() {
        let p = DefaultPolicy::new();
        let input = "ls -la\npwd\necho hello";
        let d = p.authorize(&Action::send_input("s1", input));
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn multiline_blocklist_takes_priority_over_confirm() {
        let p = DefaultPolicy::new();
        // sudo (confirm) + rm -rf / (deny) → Deny（blocklist 优先）
        let input = "sudo ls\nrm -rf /";
        let d = p.authorize(&Action::send_input("s1", input));
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn multiline_with_crlf_line_endings() {
        let p = DefaultPolicy::new();
        let input = "ls -la\r\nrm -rf /\r\n";
        let d = p.authorize(&Action::send_input("s1", input));
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn multiline_skips_empty_lines() {
        let p = DefaultPolicy::new();
        let input = "\n\n  \nls -la\n";
        let d = p.authorize(&Action::send_input("s1", input));
        assert_eq!(d, Decision::Allow);
    }

    // ── SFTP 动作测试 ────────────────────────────────────────────────

    #[test]
    fn sftp_transfer_upload_to_dev_confirms() {
        let p = DefaultPolicy::new();
        let d = p.authorize(&Action::SftpTransfer {
            direction: TransferDirection::Upload,
            local: "/tmp/file".into(),
            remote: "/dev/sda".into(),
        });
        assert_eq!(d, Decision::Confirm);
    }

    #[test]
    fn sftp_transfer_upload_to_etc_confirms() {
        let p = DefaultPolicy::new();
        let d = p.authorize(&Action::SftpTransfer {
            direction: TransferDirection::Upload,
            local: "/tmp/file".into(),
            remote: "/etc/passwd".into(),
        });
        assert_eq!(d, Decision::Confirm);
    }

    #[test]
    fn sftp_transfer_normal_allows() {
        let p = DefaultPolicy::new();
        let d = p.authorize(&Action::SftpTransfer {
            direction: TransferDirection::Upload,
            local: "/tmp/file".into(),
            remote: "/home/user/file".into(),
        });
        assert_eq!(d, Decision::Allow);

        let d = p.authorize(&Action::SftpTransfer {
            direction: TransferDirection::Download,
            local: "/tmp/file".into(),
            remote: "/etc/passwd".into(),
        });
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn sftp_remove_recursive_root_denied() {
        let p = DefaultPolicy::new();
        let d = p.authorize(&Action::SftpRemove {
            remote: "/".into(),
            recursive: true,
        });
        assert_eq!(d, Decision::Deny);

        let d = p.authorize(&Action::SftpRemove {
            remote: "/etc".into(),
            recursive: true,
        });
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn sftp_remove_recursive_non_critical_confirms() {
        let p = DefaultPolicy::new();
        let d = p.authorize(&Action::SftpRemove {
            remote: "/tmp/foo".into(),
            recursive: true,
        });
        assert_eq!(d, Decision::Confirm);
    }

    #[test]
    fn sftp_remove_non_recursive_confirms() {
        let p = DefaultPolicy::new();
        let d = p.authorize(&Action::SftpRemove {
            remote: "/tmp/file.txt".into(),
            recursive: false,
        });
        assert_eq!(d, Decision::Confirm);
    }

    #[test]
    fn sftp_chmod_777_system_dir_confirms() {
        let p = DefaultPolicy::new();
        let d = p.authorize(&Action::SftpChmod {
            remote: "/etc/passwd".into(),
            mode: "777".into(),
        });
        assert_eq!(d, Decision::Confirm);
    }

    #[test]
    fn sftp_chmod_normal_allows() {
        let p = DefaultPolicy::new();
        let d = p.authorize(&Action::SftpChmod {
            remote: "/tmp/file".into(),
            mode: "755".into(),
        });
        assert_eq!(d, Decision::Allow);
    }

    // ── PolicyManager 链式判决测试 ───────────────────────────────────

    /// 测试用 Policy：始终返回指定 Decision。
    struct StubPolicy {
        decision: Decision,
        name_str: &'static str,
    }

    impl Policy for StubPolicy {
        fn authorize(&self, _action: &Action) -> Decision {
            self.decision
        }
        fn name(&self) -> &'static str {
            self.name_str
        }
    }

    #[test]
    fn policy_manager_default_chain_has_default_policy() {
        let mgr = PolicyManager::with_default();
        assert_eq!(mgr.len(), 1);
        assert!(!mgr.is_empty());
    }

    #[test]
    fn policy_manager_all_allow_returns_allow() {
        let mut mgr = PolicyManager::new();
        mgr.add(Box::new(StubPolicy { decision: Decision::Allow, name_str: "Stub1" }));
        mgr.add(Box::new(StubPolicy { decision: Decision::Allow, name_str: "Stub2" }));
        let d = mgr.authorize(&Action::send_input("s1", "ls"));
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn policy_manager_any_deny_returns_deny() {
        let mut mgr = PolicyManager::new();
        mgr.add(Box::new(StubPolicy { decision: Decision::Allow, name_str: "Stub1" }));
        mgr.add(Box::new(StubPolicy { decision: Decision::Deny, name_str: "Stub2" }));
        mgr.add(Box::new(StubPolicy { decision: Decision::Confirm, name_str: "Stub3" }));
        let d = mgr.authorize(&Action::send_input("s1", "ls"));
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn policy_manager_confirm_without_deny_returns_confirm() {
        let mut mgr = PolicyManager::new();
        mgr.add(Box::new(StubPolicy { decision: Decision::Allow, name_str: "Stub1" }));
        mgr.add(Box::new(StubPolicy { decision: Decision::Confirm, name_str: "Stub2" }));
        mgr.add(Box::new(StubPolicy { decision: Decision::Allow, name_str: "Stub3" }));
        let d = mgr.authorize(&Action::send_input("s1", "ls"));
        assert_eq!(d, Decision::Confirm);
    }

    #[test]
    fn policy_manager_deny_takes_priority_over_confirm() {
        let mut mgr = PolicyManager::new();
        mgr.add(Box::new(StubPolicy { decision: Decision::Confirm, name_str: "Stub1" }));
        mgr.add(Box::new(StubPolicy { decision: Decision::Deny, name_str: "Stub2" }));
        let d = mgr.authorize(&Action::send_input("s1", "ls"));
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn policy_manager_deny_short_circuits() {
        // Deny 应立即返回，不调用后续 Policy
        let mut mgr = PolicyManager::new();
        mgr.add(Box::new(StubPolicy { decision: Decision::Deny, name_str: "Stub1" }));
        mgr.add(Box::new(StubPolicy { decision: Decision::Allow, name_str: "Stub2" }));
        let d = mgr.authorize(&Action::send_input("s1", "ls"));
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn policy_manager_empty_chain_returns_allow() {
        let mgr = PolicyManager::new();
        let d = mgr.authorize(&Action::send_input("s1", "rm -rf /"));
        assert_eq!(d, Decision::Allow, "空链应 Allow（无 Policy 拦截）");
    }

    #[test]
    fn policy_manager_default_chain_denies_dangerous() {
        let mgr = PolicyManager::with_default();
        let d = mgr.authorize(&Action::send_input("s1", "rm -rf /"));
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn policy_manager_default_chain_confirms_sudo() {
        let mgr = PolicyManager::with_default();
        let d = mgr.authorize(&Action::send_input("s1", "sudo ls"));
        assert_eq!(d, Decision::Confirm);
    }

    #[test]
    fn policy_manager_default_chain_allows_normal() {
        let mgr = PolicyManager::with_default();
        let d = mgr.authorize(&Action::send_input("s1", "ls -la"));
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn policy_manager_combines_default_and_stub() {
        // DefaultPolicy Allow + StubPolicy Deny → Deny
        let mut mgr = PolicyManager::with_default();
        mgr.add(Box::new(StubPolicy { decision: Decision::Deny, name_str: "BlockAll" }));
        let d = mgr.authorize(&Action::send_input("s1", "ls -la"));
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn policy_name_returns_correct_name() {
        let p = DefaultPolicy::new();
        assert_eq!(p.name(), "DefaultPolicy");
    }
}
