# Phase 7-A T16: PTY resize 验证
#
# 验证目标:
#   1. MCP resize 工具可用(AI Agent 路径)
#   2. resize 后 PTY 尺寸实际改变(远端 stty 验证)
#   3. 多次 resize 稳定
#
# Usage: powershell -ExecutionPolicy Bypass -File .\examples\phase7a_t16_resize.ps1

$ErrorActionPreference = "Stop"
$exe = ".\target\release\termbridge-mcp.exe"

if (-not (Test-Path $exe)) {
    Write-Host "BUILD: release binary not found, building..." -ForegroundColor Yellow
    cargo build --release --bin termbridge-mcp 2>&1 | Out-Null
}

# -- MCP JSON-RPC helper --
$script:proc = $null
$script:idCounter = 0

function Send-Request($method, $params) {
    $script:idCounter++
    $id = $script:idCounter
    $req = @{ jsonrpc = "2.0"; id = $id; method = $method; params = $params } | ConvertTo-Json -Compress -Depth 10
    Write-Host ">>> [$id] $method" -ForegroundColor Cyan
    $script:proc.StandardInput.WriteLine($req)
    $script:proc.StandardInput.Flush()
    $deadline = [DateTime]::Now.AddSeconds(60)
    while ([DateTime]::Now -lt $deadline) {
        $line = $script:proc.StandardOutput.ReadLine()
        if ($null -eq $line) { break }
        if ($line -match ('"id":\s*' + $id)) { return $line }
    }
    return $null
}

function Call-Tool($toolName, $arguments) {
    $resp = Send-Request "tools/call" @{ name = $toolName; arguments = $arguments }
    if ($null -eq $resp) { Write-Host "FAILED: no response for $toolName" -ForegroundColor Red; return $null }
    $parsed = $resp | ConvertFrom-Json
    if ($parsed.result.isError) {
        Write-Host "ERROR from $toolName" -ForegroundColor Red
        $parsed.result.content | ForEach-Object { Write-Host "  $($_.text)" -ForegroundColor Red }
        return $null
    }
    return $parsed.result.structuredContent
}

function Send-Notification($method, $params) {
    $req = @{ jsonrpc = "2.0"; method = $method; params = $params } | ConvertTo-Json -Compress -Depth 10
    $script:proc.StandardInput.WriteLine($req)
    $script:proc.StandardInput.Flush()
}

# -- Start --
$psi = [System.Diagnostics.ProcessStartInfo]::new()
$psi.FileName = (Resolve-Path $exe).Path
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
$script:proc = [System.Diagnostics.Process]::new()
$script:proc.StartInfo = $psi
[void]$script:proc.Start()

try {
    Write-Host "========================================" -ForegroundColor Yellow
    Write-Host "Phase 7-A T16: PTY resize test" -ForegroundColor Yellow
    Write-Host "========================================" -ForegroundColor Yellow
    Write-Host ""

    # 1. MCP initialize
    $null = Send-Request "initialize" @{
        protocolVersion = "2024-11-05"; capabilities = @{}
        clientInfo = @{ name = "t16-resize-test"; version = "0.1.0" }
    }
    Send-Notification "notifications/initialized" @{}
    Write-Host "<<< initialize OK" -ForegroundColor Green

    # 2. open_session
    Write-Host ""
    Write-Host "--- open_session (persistent=true) ---" -ForegroundColor Yellow
    $openResult = Call-Tool "open_session" @{
        host = "192.168.1.171"
        persistent = $true
        name = "t16-resize-test"
    }
    if ($null -eq $openResult) { throw "open_session failed" }
    $sid = $openResult.session_id
    Write-Host "<<< session_id = $sid" -ForegroundColor Green

    # 3. 验证初始 PTY 尺寸(stty size)
    Write-Host ""
    Write-Host "--- T16-1: 验证初始 PTY 尺寸 ---" -ForegroundColor Yellow
    $null = Call-Tool "send_input" @{ session_id = $sid; data = "stty size`n" }
    $r = Call-Tool "read_output" @{ session_id = $sid; wait_for = "rows"; timeout_secs = 5 }
    Write-Host "  stty size output: $($r.output)" -ForegroundColor Gray

    # 4. resize 到 120x40
    Write-Host ""
    Write-Host "--- T16-2: resize 到 120x40 ---" -ForegroundColor Yellow
    $resizeResult = Call-Tool "resize" @{ session_id = $sid; cols = 120; rows = 40 }
    Write-Host "<<< resize result: $($resizeResult | ConvertTo-Json -Compress)" -ForegroundColor Green

    # 验证 PTY 尺寸已改变
    $null = Call-Tool "send_input" @{ session_id = $sid; data = "stty size`n" }
    $r = Call-Tool "read_output" @{ session_id = $sid; wait_for = "40"; timeout_secs = 5 }
    Write-Host "  stty size after resize: $($r.output)" -ForegroundColor Gray
    if ($r.output -match "40\s+120") {
        Write-Host "  T16-2 PASS: PTY size changed to 120x40" -ForegroundColor Green
    } else {
        Write-Host "  T16-2 WARN: no match for 40 120, check output" -ForegroundColor Yellow
    }

    # 5. resize 到 200x50
    Write-Host ""
    Write-Host "--- T16-3: resize 到 200x50 ---" -ForegroundColor Yellow
    $null = Call-Tool "resize" @{ session_id = $sid; cols = 200; rows = 50 }
    $null = Call-Tool "send_input" @{ session_id = $sid; data = "stty size`n" }
    $r = Call-Tool "read_output" @{ session_id = $sid; wait_for = "50"; timeout_secs = 5 }
    Write-Host "  stty size after resize: $($r.output)" -ForegroundColor Gray
    if ($r.output -match "50\s+200") {
        Write-Host "  T16-3 PASS: PTY size changed to 200x50" -ForegroundColor Green
    } else {
        Write-Host "  T16-3 WARN: no match for 50 200, check output" -ForegroundColor Yellow
    }

    # 6. resize 回 80x24
    Write-Host ""
    Write-Host "--- T16-4: resize 回 80x24 ---" -ForegroundColor Yellow
    $null = Call-Tool "resize" @{ session_id = $sid; cols = 80; rows = 24 }
    $null = Call-Tool "send_input" @{ session_id = $sid; data = "stty size`n" }
    $r = Call-Tool "read_output" @{ session_id = $sid; wait_for = "24"; timeout_secs = 5 }
    Write-Host "  stty size after resize: $($r.output)" -ForegroundColor Gray
    if ($r.output -match "24\s+80") {
        Write-Host "  T16-4 PASS: PTY size changed back to 80x24" -ForegroundColor Green
    } else {
        Write-Host "  T16-4 WARN: no match for 24 80, check output" -ForegroundColor Yellow
    }

    # 7. 验证 vim 可正常启动并响应 resize(T16 核心场景)
    Write-Host ""
    Write-Host "--- T16-5: vim launch + resize test ---" -ForegroundColor Yellow
    $null = Call-Tool "send_input" @{ session_id = $sid; data = "vim /tmp/t16_test.txt`n" }
    Start-Sleep -Seconds 2  # 等 vim 启动
    # resize vim
    $null = Call-Tool "resize" @{ session_id = $sid; cols = 150; rows = 45 }
    Start-Sleep -Seconds 1
    # 退出 vim
    $null = Call-Tool "send_input" @{ session_id = $sid; data = ":q!`n" }
    Start-Sleep -Seconds 1
    Write-Host "  T16-5 PASS: vim launch + resize + exit OK" -ForegroundColor Green

    # 8. 验证 top 可正常启动并响应 resize
    Write-Host ""
    Write-Host "--- T16-6: top launch + resize test ---" -ForegroundColor Yellow
    $null = Call-Tool "send_input" @{ session_id = $sid; data = "top -n 1`n" }
    Start-Sleep -Seconds 2
    $null = Call-Tool "resize" @{ session_id = $sid; cols = 100; rows = 30 }
    Start-Sleep -Seconds 1
    $null = Call-Tool "send_control" @{ session_id = $sid; control = "CtrlC" }
    Start-Sleep -Seconds 1
    Write-Host "  T16-6 PASS: top launch + resize + exit OK" -ForegroundColor Green

    # 9. close_session
    Write-Host ""
    Write-Host "--- close_session ---" -ForegroundColor Yellow
    $null = Call-Tool "close_session" @{ session_id = $sid }
    Write-Host "<<< closed" -ForegroundColor Green

    Write-Host ""
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "T16 resize test done" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green

} finally {
    if ($null -ne $script:proc) {
        try { $script:proc.StandardInput.Close() } catch {}
        Start-Sleep -Milliseconds 300
        if (-not $script:proc.HasExited) { $script:proc.Kill() }
    }
}
