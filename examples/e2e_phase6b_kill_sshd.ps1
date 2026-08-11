# Phase 6-B 场景 1: SSH 服务端 session 进程强杀后重连
# 验证：kill sshd session → read task Err → Lost → reconnect_session → ready
# 目标：192.168.1.171
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

$pass = $true

try {
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "Phase 6-B Scenario 1: sshd kill + reconnect" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green

    $null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "phase6b-test"; version = "0.1.0" } }
    Send-Notification "notifications/initialized" @{}
    Write-Host "<<< initialized" -ForegroundColor Green

    # 1. open_session 连 171
    $open = Call-Tool 2 "open_session" @{ host = "192.168.1.171" }
    $sid = $open.session_id
    Write-Host "<<< session_id = $sid" -ForegroundColor Green

    # 2. read_output 确认 ready
    Write-Host ""
    Write-Host "--- Step 1: read_output (expect ready) ---" -ForegroundColor Yellow
    Start-Sleep -Milliseconds 800
    $r1 = Call-Tool 3 "read_output" @{ session_id = $sid; timeout_secs = 3 }
    Write-Host "<<< session_state = $($r1.session_state)" -ForegroundColor Gray
    if ($r1.session_state -ne "ready") {
        Write-Host "FAIL: expected ready, got $($r1.session_state)" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "PASS: ready" -ForegroundColor Green
    }

    # 3. 记录重连前的 session 标识（用 PID 区分新旧 shell）
    Write-Host ""
    Write-Host "--- Step 2: record shell PID (before kill) ---" -ForegroundColor Yellow
    $null = Call-Tool 4 "send_input" @{ session_id = $sid; data = "echo BEFORE_PID=$$`n" }
    Start-Sleep -Milliseconds 800
    $rPid1 = Call-Tool 5 "read_output" @{ session_id = $sid; timeout_secs = 3 }
    Write-Host "<<< output: $($rPid1.output)" -ForegroundColor Gray
    $beforeLine = ($rPid1.output -split "`n" | Where-Object { $_ -match "BEFORE_PID=" }) | Select-Object -First 1
    $beforePid = if ($beforeLine -match "BEFORE_PID=(\d+)") { $matches[1] } else { "unknown" }
    Write-Host "<<< before shell PID = $beforePid" -ForegroundColor Gray

    # 4. 远端延迟杀 sshd session 进程（nohup + sleep 2 + pkill）
    #    pkill -9 -f "sshd:.*@" 只杀 session 进程，主 sshd listener 不受影响
    Write-Host ""
    Write-Host "--- Step 3: schedule remote sshd session kill (delay 2s) ---" -ForegroundColor Yellow
    $null = Call-Tool 6 "send_input" @{ session_id = $sid; data = "nohup bash -c 'sleep 2; pkill -9 -f ""sshd:.*@""' >/dev/null 2>&1 &`n" }
    Write-Host "<<< scheduled kill in 2s, waiting for disconnect..." -ForegroundColor Gray

    # 5. 等待连接断开（kill 在 2s 后执行，read task 应在 ~2-3s 后感知）
    Start-Sleep -Seconds 4

    # 6. read_output 检测 lost
    Write-Host ""
    Write-Host "--- Step 4: read_output (expect lost) ---" -ForegroundColor Yellow
    $r2 = Call-Tool 7 "read_output" @{ session_id = $sid; timeout_secs = 2 }
    Write-Host "<<< session_state = $($r2.session_state)" -ForegroundColor Gray
    if ($r2.session_state -ne "lost") {
        Write-Host "FAIL: expected lost, got $($r2.session_state)" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "PASS: lost (sshd session killed, Agent 感知断线)" -ForegroundColor Green
    }

    # 7. reconnect_session（sshd listener 仍在，立即可重连）
    Write-Host ""
    Write-Host "--- Step 5: reconnect_session ---" -ForegroundColor Yellow
    $rc = Call-Tool 8 "reconnect_session" @{ session_id = $sid }
    Write-Host "<<< status = $($rc.status)" -ForegroundColor Gray
    Write-Host "<<< session_id = $($rc.session_id)" -ForegroundColor Gray
    if ($rc.status -ne "reconnected") {
        Write-Host "FAIL: expected reconnected, got $($rc.status)" -ForegroundColor Red
        $pass = $false
    } elseif ($rc.session_id -ne $sid) {
        Write-Host "FAIL: session_id changed" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "PASS: reconnected, session_id 复用" -ForegroundColor Green
    }

    # 8. 验证 reconnect 后是新 shell（PID 应不同）
    Write-Host ""
    Write-Host "--- Step 6: verify new shell (PID should differ) ---" -ForegroundColor Yellow
    Start-Sleep -Milliseconds 800
    $null = Call-Tool 9 "send_input" @{ session_id = $sid; data = "echo AFTER_PID=$$`n" }
    Start-Sleep -Milliseconds 800
    $rPid2 = Call-Tool 10 "read_output" @{ session_id = $sid; timeout_secs = 3 }
    Write-Host "<<< output: $($rPid2.output)" -ForegroundColor Gray
    $afterLine = ($rPid2.output -split "`n" | Where-Object { $_ -match "AFTER_PID=" }) | Select-Object -First 1
    $afterPid = if ($afterLine -match "AFTER_PID=(\d+)") { $matches[1] } else { "unknown" }
    Write-Host "<<< after shell PID = $afterPid" -ForegroundColor Gray

    if ($beforePid -ne "unknown" -and $afterPid -ne "unknown" -and $beforePid -ne $afterPid) {
        Write-Host "PASS: new shell created (before=$beforePid, after=$afterPid)" -ForegroundColor Green
    } elseif ($beforePid -eq $afterPid) {
        Write-Host "WARNING: same PID (before=$beforePid, after=$afterPid) — may be old shell reused" -ForegroundColor Yellow
    } else {
        Write-Host "NOTE: could not determine PID change (before=$beforePid, after=$afterPid)" -ForegroundColor Yellow
    }

    # 9. reconnect 后 session_state=ready
    Write-Host ""
    Write-Host "--- Step 7: read_output (expect ready) ---" -ForegroundColor Yellow
    $r3 = Call-Tool 11 "read_output" @{ session_id = $sid; timeout_secs = 2 }
    Write-Host "<<< session_state = $($r3.session_state)" -ForegroundColor Gray
    if ($r3.session_state -ne "ready") {
        Write-Host "FAIL: expected ready, got $($r3.session_state)" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "PASS: ready after reconnect" -ForegroundColor Green
    }

    # 10. close 清理
    Write-Host ""
    Write-Host "--- Cleanup: close_session ---" -ForegroundColor Yellow
    $null = Call-Tool 12 "close_session" @{ session_id = $sid }
    Write-Host "<<< closed" -ForegroundColor Gray

    Write-Host ""
    Write-Host "========================================" -ForegroundColor Green
    if ($pass) {
        Write-Host "Phase 6-B Scenario 1: ALL PASS" -ForegroundColor Green
    } else {
        Write-Host "Phase 6-B Scenario 1: SOME FAILURES" -ForegroundColor Red
    }
    Write-Host "========================================" -ForegroundColor Green
} finally {
    try { $proc.Kill() } catch {}
    $stderr = $errTask.Result
    if ($stderr) { Write-Host "`n[stderr tail]`n$($stderr.Substring([Math]::Max(0, $stderr.Length - 800)))" -ForegroundColor DarkGray }
}

if (-not $pass) { exit 1 }
