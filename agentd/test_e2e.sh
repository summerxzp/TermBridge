#!/bin/bash
set -e
source ~/.cargo/env
cd /root/TermBridge

SOCK=/tmp/termbridge_test.sock
CLI="target/release/termbridge-cli --socket $SOCK"

# 清理旧 daemon
pkill -f termbridge-agentd 2>/dev/null || true
sleep 1
rm -f $SOCK

# 启动 daemon（后台，日志到 stderr）
RUST_LOG=info target/release/termbridge-agentd serve --sock $SOCK &
DAEMON_PID=$!
sleep 1

# 确认 daemon 存活
if ! kill -0 $DAEMON_PID 2>/dev/null; then
    echo "FATAL: daemon 启动失败"
    exit 1
fi
echo "daemon PID=$DAEMON_PID"

# 1. CREATE
SID=$($CLI create --shell /bin/cat --name e2e-test 2>/dev/null | head -1)
echo "=== CREATE: session_id=$SID ==="

# 2. 检查子进程
sleep 0.5
echo "=== PS (cat + agentd) ==="
ps aux | grep -E 'cat|agentd' | grep -v grep | head -5

# 3. LIST (state=created)
echo "=== LIST (created) ==="
$CLI list 2>&1

# 4. SEND (无需 attach 即可写 PTY)
echo "=== SEND ==="
$CLI send $SID "hello-from-test" 2>&1 || echo "send failed"

# 5. READ (从 buffer 读输出)
echo "=== READ ==="
$CLI read $SID --since 0 2>&1 || echo "read failed"

# 6. ATTACH（timeout 2s 拉取增量后退出，session 转为 attached）
echo "=== ATTACH ==="
timeout 2 $CLI attach $SID --since 0 2>&1 || echo "attach exited (expected with timeout)"

# 7. DETACH (现在 session 处于 attached，可 detach)
echo "=== DETACH ==="
$CLI detach $SID 2>&1 || echo "detach failed"

# 8. LIST (state=detached)
echo "=== LIST (detached) ==="
$CLI list 2>&1

# 9. RE-ATTACH (验证 detach 后可重新 attach + 读存量输出)
echo "=== RE-ATTACH ==="
timeout 2 $CLI attach $SID --since 0 2>&1 || echo "re-attach exited (expected with timeout)"

# 10. CLOSE
echo "=== CLOSE ==="
$CLI close $SID 2>&1

# 11. LIST (应为空)
echo "=== LIST (empty) ==="
$CLI list 2>&1

# 清理
kill $DAEMON_PID 2>/dev/null || true
echo "=== DONE ==="
