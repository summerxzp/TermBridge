# Phase 3-A W3 端到端测试：persistent session 执行 Wazuh + Filebeat 配置任务
#
# 流程：open_session(persistent=true) → 逐步执行配置命令 → close_session
# 每条命令后用 echo __DONE_$?__ 标记完成，read_output wait_for 该标记
#
# 用法：.\examples\e2e_persistent_config.ps1

$ErrorActionPreference = "Stop"
$exe = ".\target\release\termbridge.exe"

if (-not (Test-Path $exe)) {
    Write-Host "BUILD: release binary not found, building..." -ForegroundColor Yellow
    cargo build --release 2>&1 | Out-Null
}

# ── 启动 termbridge.exe 子进程 ──
$psi = [System.Diagnostics.ProcessStartInfo]::new()
$psi.FileName = (Resolve-Path $exe).Path
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true

$proc = [System.Diagnostics.Process]::new()
$proc.StartInfo = $psi
[void]$proc.Start()

$errTask = $proc.StandardError.ReadToEndAsync()

# ── MCP JSON-RPC 辅助函数 ──
$script:idCounter = 0

function Send-Request($method, $params) {
    $script:idCounter++
    $id = $script:idCounter
    $req = @{
        jsonrpc = "2.0"
        id = $id
        method = $method
        params = $params
    } | ConvertTo-Json -Compress -Depth 10
    Write-Host ">>> [$id] $method" -ForegroundColor Cyan
    $proc.StandardInput.WriteLine($req)
    $proc.StandardInput.Flush()

    $deadline = [DateTime]::Now.AddSeconds(60)
    while ([DateTime]::Now -lt $deadline) {
        $line = $proc.StandardOutput.ReadLine()
        if ($null -eq $line) { break }
        if ($line -match ('"id":\s*' + $id)) {
            return $line
        }
    }
    return $null
}

function Send-Notification($method, $params) {
    $req = @{
        jsonrpc = "2.0"
        method = $method
        params = $params
    } | ConvertTo-Json -Compress -Depth 10
    $proc.StandardInput.WriteLine($req)
    $proc.StandardInput.Flush()
}

function Call-Tool($toolName, $arguments) {
    $resp = Send-Request "tools/call" @{ name = $toolName; arguments = $arguments }
    if ($null -eq $resp) {
        Write-Host "FAILED: no response for $toolName" -ForegroundColor Red
        return $null
    }
    $parsed = $resp | ConvertFrom-Json
    if ($parsed.result.isError) {
        Write-Host "ERROR from $toolName" -ForegroundColor Red
        $parsed.result.content | ForEach-Object { Write-Host "  $($_.text)" -ForegroundColor Red }
        return $null
    }
    return $parsed.result.structuredContent
}

function Send-Cmd($sessionId, $cmd) {
    Write-Host "  >> $cmd" -ForegroundColor DarkYellow
    Call-Tool "send_input" @{ session_id = $sessionId; data = "$cmd`n" } | Out-Null
}

function Read-Until($sessionId, $marker, $timeoutSecs) {
    $args = @{
        session_id = $sessionId
        wait_for = $marker
        timeout_secs = $timeoutSecs
    }
    $result = Call-Tool "read_output" $args
    if ($null -eq $result) { return $false }
    $output = $result.output
    Write-Host "  << (mode=$($result.mode), matched=$($result.matched), timed_out=$($result.timed_out), $($output.Length) bytes)" -ForegroundColor DarkGreen
    # 打印输出（缩进 + 行前缀）
    $output -split "`n" | ForEach-Object { Write-Host "     $_" -ForegroundColor Gray }
    return $result.matched
}

# ── 主流程 ──
try {
    Write-Host "========================================" -ForegroundColor Yellow
    Write-Host "Phase 3-A W3 E2E: Persistent Session Config Task" -ForegroundColor Yellow
    Write-Host "========================================" -ForegroundColor Yellow
    Write-Host ""

    # 1. MCP initialize
    $null = Send-Request "initialize" @{
        protocolVersion = "2024-11-05"
        capabilities = @{}
        clientInfo = @{ name = "w3-e2e-test"; version = "0.1.0" }
    }
    Write-Host "<<< initialize OK" -ForegroundColor Green
    Send-Notification "notifications/initialized" @{}

    # 2. open_session (persistent=true)
    Write-Host ""
    Write-Host "--- open_session (persistent=true) ---" -ForegroundColor Yellow
    $openResult = Call-Tool "open_session" @{
        host = "192.168.1.180"
        persistent = $true
        name = "wazuh-config-task"
    }
    if ($null -eq $openResult) { throw "open_session failed" }
    $sessionId = $openResult.session_id
    Write-Host "<<< session_id = $sessionId (persistent)" -ForegroundColor Green

    # 3. 验证 session 可用：发送 echo 标记
    Write-Host ""
    Write-Host "--- Verify session ---" -ForegroundColor Yellow
    Send-Cmd $sessionId "echo __SESSION_READY__"
    $ok = Read-Until $sessionId "__SESSION_READY__" 10
    if (-not $ok) { throw "session not ready: no __SESSION_READY__ marker" }

    # 4. 备份 ossec.conf
    Write-Host ""
    Write-Host "--- Step 1: Backup ossec.conf ---" -ForegroundColor Yellow
    Send-Cmd $sessionId "cp /var/ossec/etc/ossec.conf /var/ossec/etc/ossec.conf.bak.20260809; echo __DONE_`$__"
    $ok = Read-Until $sessionId "__DONE_" 10
    if (-not $ok) { Write-Host "WARNING: backup may have failed" -ForegroundColor Yellow }

    # 5. 启用 logall_json
    Write-Host ""
    Write-Host "--- Step 2: Enable logall_json ---" -ForegroundColor Yellow
    $cmd = "grep -q '<logall_json>yes' /var/ossec/etc/ossec.conf || sed -i '/<\/global>/i\  <logall_json>yes</logall_json>' /var/ossec/etc/ossec.conf; echo __DONE_`$__"
    Send-Cmd $sessionId $cmd
    $ok = Read-Until $sessionId "__DONE_" 10
    if (-not $ok) { Write-Host "WARNING: logall_json may have failed" -ForegroundColor Yellow }
    # 验证
    Send-Cmd $sessionId "grep -c logall_json /var/ossec/etc/ossec.conf; echo __VERIFIED_`$__"
    $ok = Read-Until $sessionId "__VERIFIED_" 10

    # 6. 启用 filebeat archives
    Write-Host ""
    Write-Host "--- Step 3: Enable filebeat archives ---" -ForegroundColor Yellow
    Send-Cmd $sessionId "sed -i '/^  archives:/,/^    enabled:/s/enabled: false/enabled: true/' /etc/filebeat/filebeat.yml; echo __DONE_`$__"
    $ok = Read-Until $sessionId "__DONE_" 10
    if (-not $ok) { Write-Host "WARNING: filebeat archives may have failed" -ForegroundColor Yellow }
    # 验证
    Send-Cmd $sessionId "grep -A2 'archives:' /etc/filebeat/filebeat.yml | head -5; echo __VERIFIED_`$__"
    $ok = Read-Until $sessionId "__VERIFIED_" 10

    # 7. 重启服务
    Write-Host ""
    Write-Host "--- Step 4: Restart wazuh-manager + filebeat ---" -ForegroundColor Yellow
    Send-Cmd $sessionId "systemctl restart wazuh-manager filebeat; echo __DONE_`$__"
    $ok = Read-Until $sessionId "__DONE_" 30
    if (-not $ok) { Write-Host "WARNING: restart may have failed" -ForegroundColor Yellow }

    # 8. 验证 filebeat output
    Write-Host ""
    Write-Host "--- Step 5: filebeat test output ---" -ForegroundColor Yellow
    Send-Cmd $sessionId "filebeat test output; echo __DONE_`$__"
    $ok = Read-Until $sessionId "__DONE_" 30

    # 9. 检查 archives 目录
    Write-Host ""
    Write-Host "--- Step 6: Check archives directory ---" -ForegroundColor Yellow
    Send-Cmd $sessionId "ls -l /var/ossec/logs/archives/; echo __DONE_`$__"
    $ok = Read-Until $sessionId "__DONE_" 10

    # 10. 检查服务状态
    Write-Host ""
    Write-Host "--- Step 7: Check service status ---" -ForegroundColor Yellow
    Send-Cmd $sessionId "systemctl is-active wazuh-manager filebeat; echo __DONE_`$__"
    $ok = Read-Until $sessionId "__DONE_" 10

    # 11. close_session
    Write-Host ""
    Write-Host "--- Close session ---" -ForegroundColor Yellow
    $null = Call-Tool "close_session" @{ session_id = $sessionId }
    Write-Host "<<< close_session OK" -ForegroundColor Green

    Write-Host ""
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "E2E PERSISTENT CONFIG: ALL DONE" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green

} finally {
    try { $proc.StandardInput.Close() } catch {}
    if (-not $proc.HasExited) { $proc.Kill() }
    # 打印 stderr 日志（最后 40 行）
    $errResult = $errTask.Result
    if ($errResult) {
        $errLines = $errResult -split "`n" | Where-Object { $_.Trim() } | Select-Object -Last 40
        if ($errLines) {
            Write-Host ""
            Write-Host "--- stderr (tracing logs, last 40 lines) ---" -ForegroundColor DarkGray
            $errLines | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }
        }
    }
}
