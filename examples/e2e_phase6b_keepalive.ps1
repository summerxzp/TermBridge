# Phase 6-B 场景 2: keepalive 维持空闲连接（正向验证）
# 验证：连接空闲 40s（超过 keepalive 3×10s miss 阈值）→ 仍 ready → 命令成功
# 如果 keepalive 不工作，30s 后会 disconnect → Lost
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
    Write-Host "Phase 6-B Scenario 2: keepalive idle sustain" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green

    $null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "phase6b2-test"; version = "0.1.0" } }
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

    # 3. 空闲 40s（超过 keepalive 3×10s = 30s miss 阈值）
    #    若 keepalive 不工作，30s 后会 disconnect → Lost
    Write-Host ""
    Write-Host "--- Step 2: idle 40s (keepalive must sustain connection) ---" -ForegroundColor Yellow
    $start = Get-Date
    Write-Host "<<< idle start: $($start.ToString('HH:mm:ss'))" -ForegroundColor Gray
    Start-Sleep -Seconds 40
    $end = Get-Date
    Write-Host "<<< idle end: $($end.ToString('HH:mm:ss')) (elapsed $([math]::Round(($end - $start).TotalSeconds))s)" -ForegroundColor Gray

    # 4. read_output 检测状态（应仍 ready，证明 keepalive 维持了连接）
    Write-Host ""
    Write-Host "--- Step 3: read_output after 40s idle (expect ready) ---" -ForegroundColor Yellow
    $r2 = Call-Tool 4 "read_output" @{ session_id = $sid; timeout_secs = 3 }
    Write-Host "<<< session_state = $($r2.session_state)" -ForegroundColor Gray
    if ($r2.session_state -ne "ready") {
        Write-Host "FAIL: expected ready after idle, got $($r2.session_state) — keepalive may have failed" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "PASS: ready after 40s idle (keepalive sustained connection)" -ForegroundColor Green
    }

    # 5. 验证连接仍可执行命令
    Write-Host ""
    Write-Host "--- Step 4: execute command after idle ---" -ForegroundColor Yellow
    $null = Call-Tool 5 "send_input" @{ session_id = $sid; data = "echo KEEPALIVE_OK`n" }
    Start-Sleep -Milliseconds 800
    $r3 = Call-Tool 6 "read_output" @{ session_id = $sid; timeout_secs = 3 }
    Write-Host "<<< output: $($r3.output)" -ForegroundColor Gray
    if ($r3.output -match "KEEPALIVE_OK") {
        Write-Host "PASS: command executed after idle" -ForegroundColor Green
    } else {
        Write-Host "FAIL: KEEPALIVE_OK not in output" -ForegroundColor Red
        $pass = $false
    }

    # 6. close 清理
    Write-Host ""
    Write-Host "--- Cleanup: close_session ---" -ForegroundColor Yellow
    $null = Call-Tool 7 "close_session" @{ session_id = $sid }
    Write-Host "<<< closed" -ForegroundColor Gray

    Write-Host ""
    Write-Host "========================================" -ForegroundColor Green
    if ($pass) {
        Write-Host "Phase 6-B Scenario 2: ALL PASS" -ForegroundColor Green
    } else {
        Write-Host "Phase 6-B Scenario 2: SOME FAILURES" -ForegroundColor Red
    }
    Write-Host "========================================" -ForegroundColor Green
} finally {
    try { $proc.Kill() } catch {}
    $stderr = $errTask.Result
    if ($stderr) { Write-Host "`n[stderr tail]`n$($stderr.Substring([Math]::Max(0, $stderr.Length - 600)))" -ForegroundColor DarkGray }
}

if (-not $pass) { exit 1 }
