//! ADR-0009 阶段 C1：HelperCredentialProvider（spawn helper process + IPC）。

pub mod helper;
pub use helper::HelperCredentialProvider;
