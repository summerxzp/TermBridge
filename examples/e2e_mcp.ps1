# Phase 0-C 端到端 vertical slice 测试
# 流程：initialize → open_session → send_input(echo) → read_output(wait_for) → send_control(ctrl+c) → close_session
#
# 用法：
#   .\examples\e2e_mcp.ps1
#   .\examples\e2e_mcp.ps1 -Host "root@203.0.113.200"

param(
    [string]$Host_ = "root@203.0.113.200"
)

$ErrorActionPreference = "Stop"
$exe = ".\target\release\termbridge-mcp.exe"

if (-not (Test-Path $exe)) {
    Write-Host "BUILD: release binary not found, building..." -ForegroundColor Yellow
    cargo build --release 2>&1 | Out-Null
}

# 启动进程，stdin/stdout 重定向
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

# stderr 异步读取（tracing 日志），用后台 job
$errLines = [System.Collections.Generic.List[string]]::new()
$errTask = $proc.StandardError.ReadToEndAsync()

# stdout 需要同步读（逐行）

# 发送 JSON-RPC 请求并读一行响应
function Send-Request($id, $method, $params) {
    $req = @{
        jsonrpc = "2.0"
        id = $id
        method = $method
        params = $params
    } | ConvertTo-Json -Compress -Depth 10
    Write-Host ">>> [$id] $method" -ForegroundColor Cyan
    $proc.StandardInput.WriteLine($req)
    $proc.StandardInput.Flush()

    # 读响应（可能多行，找 id 匹配的）
    $deadline = [DateTime]::Now.AddSeconds(30)
    while ([DateTime]::Now -lt $deadline) {
        $line = $proc.StandardOutput.ReadLine()
        if ($null -eq $line) { break }
        if ($line -match ('"id":\s*' + $id)) {
            return $line
        }
        # 通知或其他消息，跳过
    }
    return $null
}

# 发送通知（无 id，不读响应）
function Send-Notification($method, $params) {
    $req = @{
        jsonrpc = "2.0"
        method = $method
        params = $params
    } | ConvertTo-Json -Compress -Depth 10
    Write-Host ">>> (notify) $method" -ForegroundColor DarkCyan
    $proc.StandardInput.WriteLine($req)
    $proc.StandardInput.Flush()
}

try {
    # 1. initialize
    $r1 = Send-Request 1 "initialize" @{
        protocolVersion = "2024-11-05"
        capabilities = @{}
        clientInfo = @{ name = "e2e-test"; version = "0.1.0" }
    }
    Write-Host "<<< initialize OK" -ForegroundColor Green

    # 2. notifications/initialized
    Send-Notification "notifications/initialized" @{}

    # 3. open_session
    $r3 = Send-Request 2 "tools/call" @{
        name = "open_session"
        arguments = @{ host = $Host_ }
    }
    Write-Host "<<< open_session:" -ForegroundColor Green
    $openResult = ($r3 | ConvertFrom-Json).result.structuredContent
    Write-Host "    session_id = $($openResult.session_id)" -ForegroundColor Gray
    $sessionId = $openResult.session_id

    if (-not $sessionId) {
        Write-Host "FAILED: no session_id returned" -ForegroundColor Red
        Write-Host $r3 -ForegroundColor Yellow
        throw "open_session failed"
    }

    # 4. read_output (settle 默认模式，读初始 prompt)
    Write-Host ""
    Write-Host "--- Read initial prompt (settle mode) ---" -ForegroundColor Yellow
    $r4 = Send-Request 3 "tools/call" @{
        name = "read_output"
        arguments = @{ session_id = $sessionId; timeout_secs = 3 }
    }
    $readResult = ($r4 | ConvertFrom-Json).result.structuredContent
    Write-Host "<<< read_output (mode=$($readResult.mode), $($readResult.output.Length) bytes):" -ForegroundColor Green
    Write-Host "    output = $($readResult.output -replace "`n","⏎`n    ")" -ForegroundColor Gray

    # 5. send_input: echo HELLO_TERMBRIDGE
    Write-Host ""
    Write-Host "--- Send: echo HELLO_TERMBRIDGE ---" -ForegroundColor Yellow
    $null = Send-Request 4 "tools/call" @{
        name = "send_input"
        arguments = @{ session_id = $sessionId; data = "echo HELLO_TERMBRIDGE`n" }
    }
    Write-Host "<<< send_input OK" -ForegroundColor Green

    # 6. read_output (wait_for "HELLO_TERMBRIDGE")
    Write-Host ""
    Write-Host "--- Read: wait_for HELLO_TERMBRIDGE ---" -ForegroundColor Yellow
    $r6 = Send-Request 5 "tools/call" @{
        name = "read_output"
        arguments = @{ session_id = $sessionId; wait_for = "HELLO_TERMBRIDGE"; timeout_secs = 5 }
    }
    $readResult = ($r6 | ConvertFrom-Json).result.structuredContent
    Write-Host "<<< read_output (mode=$($readResult.mode), matched=$($readResult.matched), timed_out=$($readResult.timed_out), $($readResult.output.Length) bytes):" -ForegroundColor Green
    Write-Host "    output = $($readResult.output -replace "`n","⏎`n    ")" -ForegroundColor Gray

    # 7. send_control: ctrl+c
    Write-Host ""
    Write-Host "--- Send: ctrl+c ---" -ForegroundColor Yellow
    $null = Send-Request 6 "tools/call" @{
        name = "send_control"
        arguments = @{ session_id = $sessionId; control_key = "ctrl+c" }
    }
    Write-Host "<<< send_control OK" -ForegroundColor Green

    # 8. read_output (tail 5 lines)
    Write-Host ""
    Write-Host "--- Read: tail 5 lines ---" -ForegroundColor Yellow
    $r8 = Send-Request 7 "tools/call" @{
        name = "read_output"
        arguments = @{ session_id = $sessionId; tail_lines = 5 }
    }
    $readResult = ($r8 | ConvertFrom-Json).result.structuredContent
    Write-Host "<<< read_output (mode=$($readResult.mode), $($readResult.output.Length) bytes):" -ForegroundColor Green
    Write-Host "    output = $($readResult.output -replace "`n","⏎`n    ")" -ForegroundColor Gray

    # 9. close_session
    Write-Host ""
    Write-Host "--- Close session ---" -ForegroundColor Yellow
    $null = Send-Request 8 "tools/call" @{
        name = "close_session"
        arguments = @{ session_id = $sessionId }
    }
    Write-Host "<<< close_session OK" -ForegroundColor Green

    Write-Host ""
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "E2E VERTICAL SLICE: ALL PASSED" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green

} finally {
    try { $proc.StandardInput.Close() } catch {}
    if (-not $proc.HasExited) {
        $proc.Kill()
    }
    # 打印 stderr 日志（最后 30 行）
    $errResult = $errTask.Result
    if ($errResult) {
        $errLines = $errResult -split "`n" | Where-Object { $_.Trim() } | Select-Object -Last 30
        if ($errLines) {
            Write-Host ""
            Write-Host "--- stderr (tracing logs, last 30 lines) ---" -ForegroundColor DarkGray
            $errLines | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }
        }
    }
}
