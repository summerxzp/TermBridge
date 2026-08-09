// Phase 0-A 验证 3：ssh -G <host> 输出解析
//
// 目标：验证通过调用系统 `ssh -G <host>` 子进程复用 OpenSSH 完整 config 解析能力
//      （Include / Match / ProxyJump / Host * 等全交给 OpenSSH），TermBridge 只消费最终结果。
//
// 用法：
//   cargo run --example p0_ssh_config -- <host>
//   cargo run --example p0_ssh_config -- 192.168.88.140
//
// 验证点：
//   1. 能调用 `ssh -G <host>` 子进程并拿到 stdout
//   2. 能解析 `key value` 格式
//   3. 能提取关键字段：hostname / port / user / identityfile（多个）/ proxyjump / stricthostkeychecking
//   4. 能处理多值字段（identityfile 可能多行）

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashMap;
use std::process::Command;

#[derive(Parser)]
struct Cli {
    host: String,
}

/// 从 `ssh -G` 输出解析出的有效主机配置。
#[derive(Debug, Default)]
struct ResolvedHost {
    host: String,
    hostname: String,
    port: u16,
    user: String,
    identity_files: Vec<String>,
    proxy_jump: Option<String>,
    strict_host_key_checking: String,
    user_known_hosts_file: String,
    identities_only: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // 1. 调用 `ssh -G <host>`
    let output = Command::new("ssh")
        .arg("-G")
        .arg(&cli.host)
        .output()
        .context("failed to spawn `ssh -G`")?;

    if !output.status.success() {
        anyhow::bail!(
            "`ssh -G {}` failed: {}",
            cli.host,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("[OK] 1. `ssh -G {}` succeeded ({} bytes)", cli.host, stdout.len());

    // 2. 解析 `key value` 格式（保留多值字段的全部行）
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
        // 多值字段收集到 multi
        match key.as_str() {
            "identityfile" | "certificatefile" | "forwardagent" | "remotecommand" => {
                multi.entry(key).or_default().push(value);
            }
            _ => {
                single.insert(key, value);
            }
        }
    }
    println!("[OK] 2. parsed {} single + {} multi fields", single.len(), multi.len());

    // 3. 提取关键字段
    let resolved = ResolvedHost {
        host: cli.host.clone(),
        hostname: single.get("hostname").cloned().unwrap_or_default(),
        port: single
            .get("port")
            .and_then(|v| v.parse().ok())
            .unwrap_or(22),
        user: single.get("user").cloned().unwrap_or_default(),
        identity_files: multi.get("identityfile").cloned().unwrap_or_default(),
        proxy_jump: single.get("proxyjump").filter(|v| !v.is_empty()).cloned(),
        strict_host_key_checking: single
            .get("stricthostkeychecking")
            .cloned()
            .unwrap_or_else(|| "ask".into()),
        user_known_hosts_file: single
            .get("userknownhostsfile")
            .cloned()
            .unwrap_or_default(),
        identities_only: single
            .get("identitiesonly")
            .map(|v| v == "yes")
            .unwrap_or(false),
    };

    println!("[OK] 3. resolved key fields:");
    println!("    host         = {}", resolved.host);
    println!("    hostname     = {}", resolved.hostname);
    println!("    port         = {}", resolved.port);
    println!("    user         = {}", resolved.user);
    println!("    identityfile = {} file(s)", resolved.identity_files.len());
    for f in &resolved.identity_files {
        println!("                   - {f}");
    }
    println!("    proxyjump    = {:?}", resolved.proxy_jump);
    println!("    strict       = {}", resolved.strict_host_key_checking);
    println!("    known_hosts  = {}", resolved.user_known_hosts_file);
    println!("    idonly       = {}", resolved.identities_only);

    // 4. 多值字段验证
    if !resolved.identity_files.is_empty() {
        println!("[OK] 4. multi-value field (identityfile) parsed: {} entries", resolved.identity_files.len());
    } else {
        println!("[WARN] 4. no identityfile found");
    }

    println!("\n=== ssh -G parse strategy validated ===");
    println!("conclusion: `ssh -G` covers Include/Match/ProxyJump/Host* — no need to reimplement OpenSSH config parser");
    Ok(())
}
