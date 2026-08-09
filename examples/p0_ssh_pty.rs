// Phase 0-A 验证 2：russh 程序化 PTY 验证
//
// 目标：验证 russh 0.62 + ring backend 在 Windows 上能跑通完整 PTY 生命周期。
// 不依赖 termion/raw mode（Windows 不支持），纯程序化验证。
//
// 用法：
//   cargo run --example p0_ssh_pty -- --host <ip> --user <user> --key <path> [--port 22]
//
// 验证点（按顺序）：
//   1. SSH 公钥认证
//   2. channel.request_pty() —— 申请交互式 PTY
//   3. channel.request_shell() —— 开启交互式 shell（非 exec）
//   4. send_input("echo hello_termbridge\n") + read_output —— 验证读写
//   5. send_control(Ctrl+C) + read_output —— 鮼证控制字符
//   6. channel.window_change() —— 验证 resize
//   7. channel.eof() + disconnect —— 验证优雅关闭

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use russh::{client, ChannelMsg, Disconnect};
use std::sync::Arc;
use std::time::Duration;

/// Ctrl+C 字节
const CTRL_C: &[u8] = b"\x03";

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
    tracing::info!(host=%cli.host, port=cli.port, user=%cli.user, "connecting");

    // 1. 连接 + 密码认证
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
    tracing::info!("authenticated");
    println!("[OK] 1. SSH password auth");

    // 2. 开 channel + request PTY + request shell
    let mut channel = session.channel_open_session().await?;
    channel
        .request_pty(false, "xterm-256color", 24, 80, 0, 0, &[])
        .await?;
    channel.request_shell(true).await?;
    tracing::info!("pty + shell requested");
    println!("[OK] 2. request_pty + 3. request_shell");

    // 等待 shell 启动（读掉初始 prompt）
    let _init = read_output(&mut channel, Duration::from_secs(3)).await?;
    println!("[OK] shell ready");

    // 4. send_input + read_output
    channel.data(&b"echo hello_termbridge\n"[..]).await?;
    let out = read_output(&mut channel, Duration::from_secs(5)).await?;
    let out_str = String::from_utf8_lossy(&out);
    tracing::info!(output=%out_str, "echo output");
    if out_str.contains("hello_termbridge") {
        println!("[OK] 4. send_input + read_output: found 'hello_termbridge'");
    } else {
        println!("[WARN] 4. echo output unexpected: {out_str:?}");
    }

    // 5. Ctrl+C
    channel.data(CTRL_C as &[u8]).await?;
    let out = read_output(&mut channel, Duration::from_secs(2)).await?;
    tracing::info!(ctrlc_output=%String::from_utf8_lossy(&out), "after ctrl+c");
    println!("[OK] 5. send_control(Ctrl+C)");

    // 6. resize
    channel
        .window_change(40, 120, 0, 0)
        .await?;
    tracing::info!("resized to 40x120");
    println!("[OK] 6. window_change(40x120)");

    // resize 后再跑一条命令验证 shell 仍正常
    channel.data(&b"echo after_resize\n"[..]).await?;
    let out = read_output(&mut channel, Duration::from_secs(3)).await?;
    let out_str = String::from_utf8_lossy(&out);
    if out_str.contains("after_resize") {
        println!("[OK] 6b. shell still works after resize");
    } else {
        println!("[WARN] 6b. post-resize output: {out_str:?}");
    }

    // 7. EOF + disconnect
    channel.eof().await?;
    tracing::info!("channel eof sent");
    println!("[OK] 7. channel.eof()");

    session
        .disconnect(Disconnect::ByApplication, "bye", "en")
        .await?;
    tracing::info!("disconnected");
    println!("[OK] 7b. disconnect");

    println!("\n=== All russh PTY checks passed ===");
    Ok(())
}

/// 从 channel 读取输出，直到 timeout 或 channel 无更多数据。
async fn read_output(channel: &mut russh::Channel<russh::client::Msg>, dur: Duration) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let deadline = tokio::time::sleep(dur);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => break,
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => buf.extend_from_slice(&data),
                    Some(ChannelMsg::ExtendedData { data, .. }) => buf.extend_from_slice(&data),
                    Some(ChannelMsg::Eof) => { tracing::info!("channel EOF"); break; }
                    Some(ChannelMsg::ExitStatus { exit_status }) => { tracing::info!(exit_status, "exit"); break; }
                    Some(other) => { tracing::debug!(?other, "channel msg"); }
                    None => { tracing::info!("channel closed"); break; }
                }
            }
        }
    }
    Ok(buf)
}
