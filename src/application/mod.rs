//! TermBridge application 层：编排 domain + infrastructure，供 transport 调用。
//!
//! Phase 0-C：
//! - `hosts`：HostManager（list_hosts 从 ~/.ssh/config 枚举 Host 别名）
//! - `sessions`：SessionManager（open/send/read/control/close + list_sessions）
//!
//! Phase 1：
//! - `path_policy`：SFTP 路径策略（ADR-0005 §4）
//!
//! 关键解耦（PLAN.md §4）：Application 层定义业务接口，MCP transport 只是调用方之一。

pub mod bootstrap;
pub mod hosts;
pub mod path_policy;
pub mod policy;
pub mod sessions;
