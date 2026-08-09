#!/bin/bash
# 诊断脚本：strace 追踪 daemon 创建 session 时子进程的系统调用
source ~/.cargo/env
cd /root/TermBridge

SOCK=/tmp/termbridge_diag.sock
pkill -f termbridge-agentd 2>/dev/null || true
sleep 1
rm -f $SOCK /tmp/strace_diag.log

# 启动 daemon（strace -f 追踪所有 fork 子进程）
strace -f -o /tmp/strace_diag.log target/release/termbridge-agentd serve --sock $SOCK &
DPID=$!
sleep 1

# 创建 session
SID=$(target/release/termbridge-cli --socket $SOCK create --shell /bin/cat --name diag 2>/dev/null | head -1)
echo "session_id=$SID"
sleep 1

# 检查子进程
echo "=== PS ==="
ps aux | grep -E 'cat|agentd' | grep -v grep

# 杀 daemon
kill $DPID 2>/dev/null || true
sleep 1

# 分析 strace：找 execve 和 dup2
echo "=== STRACE: execve ==="
grep 'execve' /tmp/strace_diag.log | tail -10
echo "=== STRACE: dup2 ==="
grep 'dup2' /tmp/strace_diag.log | tail -10
echo "=== STRACE: openpty/posix_openpt ==="
grep -E 'openat.*ptmx|posix_openpt|ioctl.*TIOC' /tmp/strace_diag.log | tail -10
echo "=== STRACE: child exit ==="
grep -E 'exit_group|_exit' /tmp/strace_diag.log | tail -10
