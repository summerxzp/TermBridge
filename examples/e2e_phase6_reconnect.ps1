# Phase 6-A e2e: Session 断线感知 + 手动重连（ADR-0010）
# 验证流程：open_session → read_output(ready) → exit 触发 Lost → read_output(session_state=lost) → reconnect_session → read_output(reconnected)
$ErrorActionPreference = "Stop"
$exe = ".\target\release\termbridge-mcp.exe"

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

function Send-Request($id, $method, $params) {
    $req = @{ jsonrpc = "2.0"; id = $id; method = $method; params = $params } | ConvertTo-Json -Compress -Depth 10
    Write-Host ">>> [$id] $method" -ForegroundColor Cyan
    $proc.StandardInput.WriteLine($req)
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::Now.AddSeconds(30)
    while ([DateTime]::Now -lt $deadline) {
        $line = $proc.StandardOutput.ReadLine()
        if ($null -eq $line) { break }
        if ($line -match ('"id":\s*' + $id)) { return $line }
    }
    return $null
}

function Send-Notification($method, $params) {
    $req = @{ jsonrpc = "2.0"; method = $method; params = $params } | ConvertTo-Json -Compress -Depth 10
    $proc.StandardInput.WriteLine($req)
    $proc.StandardInput.Flush()
}

function Call-Tool($id, $name, $arguments) {
    $r = Send-Request $id "tools/call" @{ name = $name; arguments = $arguments }
    if ($null -eq $r) { throw "no response for $name" }
    $parsed = $r | ConvertFrom-Json
    if ($parsed.result.isError) {
        $parsed.result.content | ForEach-Object { Write-Host "  ERR: $($_.text)" -ForegroundColor Red }
        throw "$name failed"
    }
    return $parsed.result.structuredContent
}

function Call-ToolRaw($id, $name, $arguments) {
    # 不抛错，返回原始 parsed 结果（用于断言错误码）
    $r = Send-Request $id "tools/call" @{ name = $name; arguments = $arguments }
    if ($null -eq $r) { throw "no response for $name" }
    return ($r | ConvertFrom-Json).result
}

$pass = $true

try {
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "Phase 6-A Reconnect E2E (ADR-0010)" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green

    $null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "phase6-test"; version = "0.1.0" } }
    Send-Notification "notifications/initialized" @{}
    Write-Host "<<< initialized" -ForegroundColor Green

    # 1. open_session 连 171
    $open = Call-Tool 2 "open_session" @{ host = "192.168.1.171" }
    $sid = $open.session_id
    Write-Host "<<< session_id = $sid" -ForegroundColor Green

    # 2. read_output 确认 ready，看到 prompt
    Write-Host ""
    Write-Host "--- Step 1: read_output (expect ready) ---" -ForegroundColor Yellow
    Start-Sleep -Milliseconds 800
    $r1 = Call-Tool 3 "read_output" @{ session_id = $sid; timeout_secs = 3 }
    Write-Host "<<< session_state = $($r1.session_state)" -ForegroundColor Gray
    Write-Host "<<< output (first 100 chars): $($r1.output.Substring(0, [Math]::Min(100, $r1.output.Length)))" -ForegroundColor Gray
    if ($r1.session_state -ne "ready") {
        Write-Host "FAIL: expected session_state=ready, got $($r1.session_state)" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "PASS: session_state=ready" -ForegroundColor Green
    }

    # 3. send_input "exit\n" 让 shell 退出 → SSH channel EOF → read task None → Lost
    Write-Host ""
    Write-Host "--- Step 2: send 'exit' to trigger Lost ---" -ForegroundColor Yellow
    $null = Call-Tool 4 "send_input" @{ session_id = $sid; data = "exit`n" }
    Write-Host "<<< sent 'exit', waiting for Lost..." -ForegroundColor Gray
    Start-Sleep -Seconds 2

    # 4. read_output 检测 session_state=lost
    Write-Host ""
    Write-Host "--- Step 3: read_output (expect lost) ---" -ForegroundColor Yellow
    $r2 = Call-Tool 5 "read_output" @{ session_id = $sid; timeout_secs = 2 }
    Write-Host "<<< session_state = $($r2.session_state)" -ForegroundColor Gray
    Write-Host "<<< output (first 100 chars): $($r2.output.Substring(0, [Math]::Min(100, $r2.output.Length)))" -ForegroundColor Gray
    if ($r2.session_state -ne "lost") {
        Write-Host "FAIL: expected session_state=lost, got $($r2.session_state)" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "PASS: session_state=lost (Agent can感知断线)" -ForegroundColor Green
    }

    # 5. 验证 Lost 状态下 send_input 返回 SESSION_CLOSED
    Write-Host ""
    Write-Host "--- Step 4: send_input on Lost (expect SESSION_CLOSED) ---" -ForegroundColor Yellow
    $r3 = Call-ToolRaw 6 "send_input" @{ session_id = $sid; data = "echo should_fail`n" }
    if ($r3.isError) {
        $errCode = $r3.structuredContent.code
        Write-Host "<<< error code = $errCode" -ForegroundColor Gray
        if ($errCode -eq "SESSION_CLOSED") {
            Write-Host "PASS: Lost 状态 send_input 返回 SESSION_CLOSED" -ForegroundColor Green
        } else {
            Write-Host "FAIL: expected SESSION_CLOSED, got $errCode" -ForegroundColor Red
            $pass = $false
        }
    } else {
        Write-Host "FAIL: expected error, got success" -ForegroundColor Red
        $pass = $false
    }

    # 6. reconnect_session
    Write-Host ""
    Write-Host "--- Step 5: reconnect_session ---" -ForegroundColor Yellow
    $rc = Call-Tool 7 "reconnect_session" @{ session_id = $sid }
    Write-Host "<<< status = $($rc.status)" -ForegroundColor Gray
    Write-Host "<<< session_id = $($rc.session_id)" -ForegroundColor Gray
    Write-Host "<<< host = $($rc.host)" -ForegroundColor Gray
    Write-Host "<<< cwd_restored = $($rc.cwd_restored)" -ForegroundColor Gray
    if ($rc.status -ne "reconnected") {
        Write-Host "FAIL: expected status=reconnected, got $($rc.status)" -ForegroundColor Red
        $pass = $false
    } elseif ($rc.session_id -ne $sid) {
        Write-Host "FAIL: session_id changed (expected $sid, got $($rc.session_id))" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "PASS: reconnected, session_id 复用" -ForegroundColor Green
    }

    # 7. read_output 确认 reconnected，新 session 可用
    Write-Host ""
    Write-Host "--- Step 6: read_output after reconnect (expect ready) ---" -ForegroundColor Yellow
    Start-Sleep -Milliseconds 800
    $r4 = Call-Tool 8 "read_output" @{ session_id = $sid; timeout_secs = 3 }
    Write-Host "<<< session_state = $($r4.session_state)" -ForegroundColor Gray
    Write-Host "<<< output (first 100 chars): $($r4.output.Substring(0, [Math]::Min(100, $r4.output.Length)))" -ForegroundColor Gray
    if ($r4.session_state -ne "ready") {
        Write-Host "FAIL: expected session_state=ready after reconnect, got $($r4.session_state)" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "PASS: reconnected session is ready" -ForegroundColor Green
    }

    # 8. 验证 reconnect 后能正常执行命令
    Write-Host ""
    Write-Host "--- Step 7: execute command after reconnect ---" -ForegroundColor Yellow
    $null = Call-Tool 9 "send_input" @{ session_id = $sid; data = "echo RECONNECT_VERIFY_OK`n" }
    Start-Sleep -Milliseconds 800
    $r5 = Call-Tool 10 "read_output" @{ session_id = $sid; timeout_secs = 3 }
    Write-Host "<<< output: $($r5.output)" -ForegroundColor Gray
    if ($r5.output -match "RECONNECT_VERIFY_OK") {
        Write-Host "PASS: command executed successfully after reconnect" -ForegroundColor Green
    } else {
        Write-Host "FAIL: RECONNECT_VERIFY_OK not found in output" -ForegroundColor Red
        $pass = $false
    }

    # 9. reconnect on Ready session → expect not_lost
    Write-Host ""
    Write-Host "--- Step 8: reconnect on Ready (expect not_lost) ---" -ForegroundColor Yellow
    $rc2 = Call-Tool 11 "reconnect_session" @{ session_id = $sid }
    Write-Host "<<< status = $($rc2.status)" -ForegroundColor Gray
    if ($rc2.status -eq "not_lost") {
        Write-Host "PASS: Ready 状态 reconnect 返回 not_lost" -ForegroundColor Green
    } else {
        Write-Host "FAIL: expected not_lost, got $($rc2.status)" -ForegroundColor Red
        $pass = $false
    }

    # 10. close_session 清理
    Write-Host ""
    Write-Host "--- Cleanup: close_session ---" -ForegroundColor Yellow
    $null = Call-Tool 12 "close_session" @{ session_id = $sid }
    Write-Host "<<< closed" -ForegroundColor Gray

    Write-Host ""
    Write-Host "========================================" -ForegroundColor Green
    if ($pass) {
        Write-Host "Phase 6-A E2E: ALL PASS" -ForegroundColor Green
    } else {
        Write-Host "Phase 6-A E2E: SOME FAILURES (see above)" -ForegroundColor Red
    }
    Write-Host "========================================" -ForegroundColor Green
} finally {
    try { $proc.Kill() } catch {}
    $stderr = $errTask.Result
    if ($stderr) { Write-Host "`n[stderr tail]`n$($stderr.Substring([Math]::Max(0, $stderr.Length - 500)))" -ForegroundColor DarkGray }
}

if (-not $pass) { exit 1 }
