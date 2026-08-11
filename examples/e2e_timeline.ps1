# Phase 4-A Timeline e2e 验证
# 流程：open_session → send_input(echo) → read_output(wait_for) → send_control(ctrl+c) → get_session_timeline → 验证事件
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
        Write-Host "ERROR from $name" -ForegroundColor Red
        $parsed.result.content | ForEach-Object { Write-Host "  $($_.text)" -ForegroundColor Red }
        throw "$name failed"
    }
    return $parsed.result.structuredContent
}

try {
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "Phase 4-A Timeline E2E Verification" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green

    # 1. initialize
    $null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "timeline-test"; version = "0.1.0" } }
    Send-Notification "notifications/initialized" @{}
    Write-Host "<<< initialized" -ForegroundColor Green

    # 2. open_session
    $open = Call-Tool 2 "open_session" @{ host = "192.168.1.180" }
    $sid = $open.session_id
    Write-Host "<<< session_id = $sid" -ForegroundColor Green

    # 3. read initial prompt (settle)
    $null = Call-Tool 3 "read_output" @{ session_id = $sid; timeout_secs = 3 }
    Write-Host "<<< initial prompt read" -ForegroundColor Green

    # 4. send_input: echo TIMELINE_TEST_001
    $null = Call-Tool 4 "send_input" @{ session_id = $sid; data = "echo TIMELINE_TEST_001`n" }
    Write-Host "<<< sent echo TIMELINE_TEST_001" -ForegroundColor Green

    # 5. read_output wait_for
    $r5 = Call-Tool 5 "read_output" @{ session_id = $sid; wait_for = "TIMELINE_TEST_001"; timeout_secs = 5 }
    Write-Host "<<< wait_for matched=$($r5.matched)" -ForegroundColor Green

    # 6. send_input: echo TIMELINE_TEST_002
    $null = Call-Tool 6 "send_input" @{ session_id = $sid; data = "echo TIMELINE_TEST_002`n" }
    Write-Host "<<< sent echo TIMELINE_TEST_002" -ForegroundColor Green

    # 7. read_output wait_for
    $r7 = Call-Tool 7 "read_output" @{ session_id = $sid; wait_for = "TIMELINE_TEST_002"; timeout_secs = 5 }
    Write-Host "<<< wait_for matched=$($r7.matched)" -ForegroundColor Green

    # 8. send_control: ctrl+c
    $null = Call-Tool 8 "send_control" @{ session_id = $sid; control_key = "ctrl+c" }
    Write-Host "<<< sent ctrl+c" -ForegroundColor Green

    # 9. get_session_timeline — 关键验证
    Write-Host ""
    Write-Host "--- get_session_timeline ---" -ForegroundColor Yellow
    $tl = Call-Tool 9 "get_session_timeline" @{ session_id = $sid }
    $events = $tl.events
    Write-Host "<<< timeline: $($events.Count) events" -ForegroundColor Green

    # 10. 验证事件类型
    $commands = @($events | Where-Object { $_.type -eq "command" })
    $outputs = @($events | Where-Object { $_.type -eq "output" })
    $controls = @($events | Where-Object { $_.type -eq "control" })
    $stateChanges = @($events | Where-Object { $_.type -eq "state_change" })

    Write-Host ""
    Write-Host "=== Event Breakdown ===" -ForegroundColor Yellow
    Write-Host "  command:      $($commands.Count)" -ForegroundColor Gray
    Write-Host "  output:       $($outputs.Count)" -ForegroundColor Gray
    Write-Host "  control:      $($controls.Count)" -ForegroundColor Gray
    Write-Host "  state_change: $($stateChanges.Count)" -ForegroundColor Gray

    # 11. 验证命令事件
    $pass = $true
    if ($commands.Count -lt 2) {
        Write-Host "FAIL: expected >= 2 command events, got $($commands.Count)" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "  command[0].input: $($commands[0].input)" -ForegroundColor Gray
        Write-Host "  command[0].command_id: $($commands[0].command_id)" -ForegroundColor Gray
        Write-Host "  command[0].cursor_before: $($commands[0].cursor_before)" -ForegroundColor Gray
        if ($commands[0].input -notmatch "TIMELINE_TEST_001") {
            Write-Host "FAIL: command[0] should contain TIMELINE_TEST_001" -ForegroundColor Red
            $pass = $false
        }
    }

    # 12. 验证 output 事件
    if ($outputs.Count -eq 0) {
        Write-Host "FAIL: expected >= 1 output event" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "  output[0].cursor_start: $($outputs[0].cursor_start)" -ForegroundColor Gray
        Write-Host "  output[0].cursor_end: $($outputs[0].cursor_end)" -ForegroundColor Gray
        Write-Host "  output[0].bytes: $($outputs[0].bytes)" -ForegroundColor Gray
    }

    # 13. 验证 control 事件
    if ($controls.Count -lt 1) {
        Write-Host "FAIL: expected >= 1 control event" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "  control[0].control: $($controls[0].control)" -ForegroundColor Gray
        if ($controls[0].control -ne "ctrl+c") {
            Write-Host "FAIL: control[0] should be 'ctrl+c', got '$($controls[0].control)'" -ForegroundColor Red
            $pass = $false
        }
    }

    # 14. 验证 state_change 事件
    if ($stateChanges.Count -eq 0) {
        Write-Host "FAIL: expected >= 1 state_change event" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "  state_change[0].from: $($stateChanges[0].from) -> to: $($stateChanges[0].to)" -ForegroundColor Gray
    }

    # 15. 验证 limit 参数
    $tlLimited = Call-Tool 10 "get_session_timeline" @{ session_id = $sid; limit = 3 }
    if ($tlLimited.events.Count -ne 3) {
        Write-Host "FAIL: limit=3 should return 3 events, got $($tlLimited.events.Count)" -ForegroundColor Red
        $pass = $false
    } else {
        Write-Host "<<< limit=3 returned 3 events (OK)" -ForegroundColor Green
    }

    # 16. close_session
    $null = Call-Tool 11 "close_session" @{ session_id = $sid }
    Write-Host "<<< closed" -ForegroundColor Green

    Write-Host ""
    if ($pass) {
        Write-Host "========================================" -ForegroundColor Green
        Write-Host "TIMELINE E2E: ALL PASSED" -ForegroundColor Green
        Write-Host "========================================" -ForegroundColor Green
    } else {
        Write-Host "========================================" -ForegroundColor Red
        Write-Host "TIMELINE E2E: FAILED" -ForegroundColor Red
        Write-Host "========================================" -ForegroundColor Red
    }

} finally {
    try { $proc.StandardInput.Close() } catch {}
    if (-not $proc.HasExited) { $proc.Kill() }
    $errResult = $errTask.Result
    if ($errResult) {
        $errLines = $errResult -split "`n" | Where-Object { $_.Trim() } | Select-Object -Last 15
        if ($errLines) {
            Write-Host ""
            Write-Host "--- stderr (last 15 lines) ---" -ForegroundColor DarkGray
            $errLines | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }
        }
    }
}
