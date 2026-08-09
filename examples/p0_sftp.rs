// Phase 0-A 验证 4：russh-sftp upload/download
//
// 目标：验证 russh-sftp 2.4.0 能在 russh 0.62 + ring backend 上跑通基本文件传输。
//
// 用法：
//   cargo run --example p0_sftp -- --host <ip> --user <user> --password <pwd>
//
// 验证点：
//   1. SSH 连接 + 密码认证
//   2. channel.request_subsystem("sftp") —— 请求 SFTP 子系统
//   3. SftpSession::new —— 建立 SFTP 会话
//   4. upload：创建文件 + 写入内容
//   5. download：重新打开文件 + 读取内容 + 验证一致
//   6. 清理：删除测试文件

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use russh::{client, Disconnect};
use russh_sftp::{client::SftpSession, protocol::OpenFlags};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

#[derive(Parser)]
struct Cli {
    #[clap(long)]
    host: String,
    #[clap(long, default_value_t = 22)]
    port: u16,
    #[clap(long)]
    user: String,
    #[clap(long)]
    password: String,
}

struct Client;

impl client::Handler for Client {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // Phase 0 原型：接受任意 host key。Phase 1 必须改为 known_hosts 校验。
        Ok(true)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    tracing::info!(host=%cli.host, user=%cli.user, "connecting");

    // 1. SSH 连接 + 密码认证
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(15)),
        ..<_>::default()
    });
    let mut session = client::connect(config, (cli.host.as_str(), cli.port), Client)
        .await
        .context("tcp connect")?;

    let auth_res = session
        .authenticate_password(&cli.user, &cli.password)
        .await
        .context("authenticate")?;
    if !auth_res.success() {
        return Err(anyhow!("authentication failed"));
    }
    println!("[OK] 1. SSH password auth");

    // 2. 开 channel + 请求 SFTP 子系统
    let channel = session.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    println!("[OK] 2. request_subsystem(sftp)");

    // 3. 建立 SFTP 会话
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .context("SftpSession::new")?;
    println!("[OK] 3. SftpSession established");

    let cwd = sftp.canonicalize(".").await?;
    tracing::info!(?cwd, "current remote dir");

    // 4. upload：创建文件 + 写入内容
    let remote_path = "/tmp/termbridge_p0_sftp_test.txt";
    let upload_content = b"hello from TermBridge Phase 0-A sftp upload\n";

    let mut file = sftp
        .open_with_flags(
            remote_path,
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE | OpenFlags::READ,
        )
        .await
        .context("open_with_flags for upload")?;
    file.write_all(upload_content).await?;
    file.flush().await?;
    file.shutdown().await?;
    println!(
        "[OK] 4. upload: wrote {} bytes to {remote_path}",
        upload_content.len()
    );

    // 5. download：重新打开文件 + 读取 + 验证一致
    let mut file = sftp
        .open_with_flags(remote_path, OpenFlags::READ)
        .await
        .context("open_with_flags for download")?;

    let mut downloaded = Vec::new();
    file.read_to_end(&mut downloaded).await?;
    file.shutdown().await?;

    if downloaded == upload_content {
        println!("[OK] 5. download: {} bytes match upload", downloaded.len());
    } else {
        println!(
            "[FAIL] 5. download mismatch: got {:?} expected {:?}",
            downloaded, upload_content
        );
    }

    // 6. 清理
    sftp.remove_file(remote_path).await?;
    println!("[OK] 6. cleanup: removed {remote_path}");

    // 关闭
    session
        .disconnect(Disconnect::ByApplication, "bye", "en")
        .await?;
    println!("[OK] 7. disconnect");

    println!("\n=== russh-sftp upload/download validated ===");
    Ok(())
}
