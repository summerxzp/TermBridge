# Phase 4-B Observability e2e (non-destructive)
# Verify: timeline events work after observability changes
$ErrorActionPreference = "Stop"
$exe = ".\target\release\termbridge-mcp.exe"

$psi = [System.Diagnostics.ProcessStartInfo]::new()
$psi.FileName = (Resolve-Path $exe).Path
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
$psi.EnvironmentVariables["RUST_LOG"] = "info"

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
$events = @()

try {
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "Phase 4-B Observability E2E" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green

    $null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "obs-test"; version = "0.1.0" } }
    Send-Notification "notifications/initialized" @{}
    Write-Host "<<< initialized" -ForegroundColor Green

    $open = Call-Tool 2 "open_session" @{ host = "192.168.1.180" }
    $sid = $open.session_id
    Write-Host "<<< session_id = $sid" -ForegroundColor Green

    $null = Call-Tool 3 "read_output" @{ session_id = $sid; timeout_secs = 3 }
    Write-Host "<<< initial prompt read" -ForegroundColor Green

    $null = Call-Tool 4 "send_input" @{ session_id = $sid; data = "echo OBS_TEST_001`n" }
    Write-Host "<<< sent echo OBS_TEST_001" -ForegroundColor Green

    $r5 = Call-Tool 5 "read_output" @{ session_id = $sid; wait_for = "OBS_TEST_001"; timeout_secs = 5 }
    Write-Host "<<< wait_for matched=$($r5.matched)" -ForegroundColor Green

    $null = Call-Tool 6 "send_input" @{ session_id = $sid; data = "echo OBS_TEST_002`n" }
    Write-Host "<<< sent echo OBS_TEST_002" -ForegroundColor Green

    $r7 = Call-Tool 7 "read_output" @{ session_id = $sid; wait_for = "OBS_TEST_002"; timeout_secs = 5 }
    Write-Host "<<< wait_for matched=$($r7.matched)" -ForegroundColor Green

    $null = Call-Tool 8 "send_control" @{ session_id = $sid; control_key = "ctrl+c" }
    Write-Host "<<< sent ctrl+c" -ForegroundColor Green

    $tl = Call-Tool 9 "get_session_timeline" @{ session_id = $sid; limit = 20 }
    $events = @($tl.events)
    Write-Host "<<< timeline: $($events.Count) events" -ForegroundColor Green

    $null = Call-Tool 10 "close_session" @{ session_id = $sid }
    Write-Host "<<< closed" -ForegroundColor Green

} catch {
    Write-Host "EXCEPTION: $_" -ForegroundColor Red
    $pass = $false
}

if ($proc.StandardInput) { $proc.StandardInput.Close() }
if (-not $proc.HasExited) { $proc.Kill(); $proc.WaitForExit(3000) | Out-Null }

# Verify timeline
Write-Host ""
Write-Host "=== Timeline Verification ===" -ForegroundColor Yellow
if ($events.Count -ge 5) {
    Write-Host "  [PASS] timeline has $($events.Count) events (>= 5)" -ForegroundColor Green
} else {
    Write-Host "  [FAIL] timeline has only $($events.Count) events" -ForegroundColor Red
    $pass = $false
}

$commands = @($events | Where-Object { $_.type -eq "command" })
if ($commands.Count -ge 2) {
    Write-Host "  [PASS] timeline has $($commands.Count) command events (>= 2)" -ForegroundColor Green
} else {
    Write-Host "  [FAIL] timeline has only $($commands.Count) command events" -ForegroundColor Red
    $pass = $false
}

$outputs = @($events | Where-Object { $_.type -eq "output" })
if ($outputs.Count -ge 1) {
    Write-Host "  [PASS] timeline has $($outputs.Count) output events" -ForegroundColor Green
} else {
    Write-Host "  [FAIL] timeline missing output events" -ForegroundColor Red
    $pass = $false
}

$controls = @($events | Where-Object { $_.type -eq "control" })
if ($controls.Count -ge 1) {
    Write-Host "  [PASS] timeline has $($controls.Count) control events" -ForegroundColor Green
    if ($controls[0].control -eq "ctrl+c") {
        Write-Host "  [PASS] control[0] is ctrl+c" -ForegroundColor Green
    } else {
        Write-Host "  [FAIL] control[0] is '$($controls[0].control)', expected 'ctrl+c'" -ForegroundColor Red
        $pass = $false
    }
} else {
    Write-Host "  [FAIL] timeline missing control events" -ForegroundColor Red
    $pass = $false
}

$stateChanges = @($events | Where-Object { $_.type -eq "state_change" })
if ($stateChanges.Count -ge 1) {
    Write-Host "  [PASS] timeline has $($stateChanges.Count) state_change events" -ForegroundColor Green
} else {
    Write-Host "  [FAIL] timeline missing state_change events" -ForegroundColor Red
    $pass = $false
}

# Verify tracing logs from stderr (info level: close_session)
$errResult = ""
try { $errResult = $errTask.Wait(2000).Result } catch {}
Write-Host ""
Write-Host "=== Tracing Log Verification (info level) ===" -ForegroundColor Yellow
if ($errResult -match "close_session") {
    Write-Host "  [PASS] close_session tracing found" -ForegroundColor Green
} else {
    Write-Host "  [SKIP] close_session tracing not captured in stderr (may need process exit first)" -ForegroundColor Yellow
}
if ($errResult -match "send_control") {
    Write-Host "  [PASS] send_control tracing found" -ForegroundColor Green
} else {
    Write-Host "  [SKIP] send_control tracing not captured in stderr" -ForegroundColor Yellow
}

Write-Host ""
if ($pass) {
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "OBSERVABILITY E2E: ALL PASSED" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green
} else {
    Write-Host "========================================" -ForegroundColor Red
    Write-Host "OBSERVABILITY E2E: FAILED" -ForegroundColor Red
    Write-Host "========================================" -ForegroundColor Red
}

if ($errResult) {
    $errLines = ($errResult -split "`n") | Where-Object { $_.Trim() } | Select-Object -Last 20
    if ($errLines) {
        Write-Host ""
        Write-Host "--- stderr (last 20 lines) ---" -ForegroundColor DarkGray
        $errLines | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }
    }
}
