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
ps aux | grep -E 'cat|agentd' | grep -v grep

# 3. LIST
echo "=== LIST ==="
$CLI list 2>&1

# 4. SEND
echo "=== SEND ==="
$CLI send $SID "hello-from-test" 2>&1 || echo "send failed"

# 5. READ
echo "=== READ ==="
$CLI read $SID 0 2>&1 || echo "read failed"

# 6. DETACH
echo "=== DETACH ==="
$CLI detach $SID 2>&1 || echo "detach failed"

# 7. LIST after detach
echo "=== LIST after detach ==="
$CLI list 2>&1

# 8. ATTACH
echo "=== ATTACH ==="
$CLI attach $SID 0 2>&1 || echo "attach failed"

# 9. CLOSE
echo "=== CLOSE ==="
$CLI close $SID 2>&1

# 10. LIST after close
echo "=== LIST after close ==="
$CLI list 2>&1

# 清理
kill $DAEMON_PID 2>/dev/null || true
echo "=== DONE ==="
