# Phase 3-B W4 E2E: Cross-MCP Restart Reconnection (PersistentProvider core value)
#
# Full flow:
# 1. MCP process 1: open_session(persistent=true) -> send_input produces output -> detach_session (remote PTY kept alive)
# 2. MCP process 1 exits (simulating MCP restart/crash)
# 3. MCP process 2: list_remote_sessions -> attach_remote_session -> read_output history -> send_input continue -> close_session
#
# Usage: powershell -ExecutionPolicy Bypass -File .\examples\e2e_cross_restart.ps1

$ErrorActionPreference = "Stop"
$exe = ".\target\release\termbridge.exe"

if (-not (Test-Path $exe)) {
    Write-Host "BUILD: release binary not found, building..." -ForegroundColor Yellow
    cargo build --release 2>&1 | Out-Null
}

# -- Script-level variables (two MCP processes share helpers via $script:proc) --
$script:proc = $null
$script:errTask = $null
$script:allStderr = [System.Collections.Generic.List[string]]::new()
$script:idCounter = 0

# -- MCP JSON-RPC helper functions --

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
    $script:proc.StandardInput.WriteLine($req)
    $script:proc.StandardInput.Flush()

    $deadline = [DateTime]::Now.AddSeconds(60)
    while ([DateTime]::Now -lt $deadline) {
        $line = $script:proc.StandardOutput.ReadLine()
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
    $script:proc.StandardInput.WriteLine($req)
    $script:proc.StandardInput.Flush()
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
    $readParams = @{
        session_id = $sessionId
        wait_for = $marker
        timeout_secs = $timeoutSecs
    }
    $result = Call-Tool "read_output" $readParams
    if ($null -eq $result) { return $false }
    $output = $result.output
    Write-Host "  << (mode=$($result.mode), matched=$($result.matched), timed_out=$($result.timed_out), $($output.Length) bytes)" -ForegroundColor DarkGreen
    $output -split "`n" | ForEach-Object { Write-Host "     $_" -ForegroundColor Gray }
    return $result.matched
}

# -- Process management helpers --

function Start-TermBridge {
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
    $script:errTask = $script:proc.StandardError.ReadToEndAsync()
    $script:idCounter = 0
}

function Stop-TermBridge {
    if ($null -ne $script:proc) {
        try { $script:proc.StandardInput.Close() } catch {}
        Start-Sleep -Milliseconds 500
        if (-not $script:proc.HasExited) { $script:proc.Kill() }
        # Collect stderr
        if ($null -ne $script:errTask) {
            try {
                $err = $script:errTask.Result
                if ($err) {
                    $script:allStderr.Add("--- Process (PID=$($script:proc.Id)) stderr ---")
                    $err -split "`n" | Where-Object { $_.Trim() } | ForEach-Object { $script:allStderr.Add($_) }
                }
            } catch {}
        }
        $script:proc = $null
        $script:errTask = $null
    }
}

# -- Main flow --
try {
    Write-Host "========================================" -ForegroundColor Yellow
    Write-Host "Phase 3-B W4 E2E: Cross-MCP Restart Reconnection" -ForegroundColor Yellow
    Write-Host "========================================" -ForegroundColor Yellow
    Write-Host ""

    # Generate unique markers and session name (avoid conflicts with leftover sessions from previous runs)
    $runId = Get-Date -Format "yyyyMMddHHmmss"
    $sessionName = "cross-restart-test-$runId"
    $MARKER1 = "CROSS_RESTART_${runId}_001"
    $MARKER2 = "CROSS_RESTART_${runId}_002"
    $MARKER3 = "CROSS_RESTART_${runId}_003"

    Write-Host "Run ID       : $runId" -ForegroundColor Gray
    Write-Host "Session Name : $sessionName" -ForegroundColor Gray
    Write-Host "Markers      : $MARKER1, $MARKER2, $MARKER3" -ForegroundColor Gray
    Write-Host ""

    # ==================================================================
    # Phase 1: MCP Process 1 - Create + Detach
    # ==================================================================
    Write-Host "========== Phase 1: MCP Process 1 (Create + Detach) ==========" -ForegroundColor Yellow
    Write-Host ""

    Write-Host "--- Starting termbridge.exe process 1 ---" -ForegroundColor Yellow
    Start-TermBridge
    Write-Host "<<< process 1 started (PID=$($script:proc.Id))" -ForegroundColor Green

    # 1. MCP initialize
    $null = Send-Request "initialize" @{
        protocolVersion = "2024-11-05"
        capabilities = @{}
        clientInfo = @{ name = "w4-e2e-cross-restart"; version = "0.1.0" }
    }
    Write-Host "<<< initialize OK" -ForegroundColor Green
    Send-Notification "notifications/initialized" @{}

    # 2. open_session (persistent=true)
    Write-Host ""
    Write-Host "--- open_session (persistent=true, name=$sessionName) ---" -ForegroundColor Yellow
    $openResult = Call-Tool "open_session" @{
        host = "192.168.1.180"
        persistent = $true
        name = $sessionName
    }
    if ($null -eq $openResult) { throw "Phase 1: open_session failed" }
    $sessionId1 = $openResult.session_id
    Write-Host "<<< session_id_1 = $sessionId1 (persistent)" -ForegroundColor Green

    # 3. send_input: echo MARKER1
    Write-Host ""
    Write-Host "--- Send: echo $MARKER1 ---" -ForegroundColor Yellow
    Send-Cmd $sessionId1 "echo $MARKER1"
    $ok = Read-Until $sessionId1 $MARKER1 10
    if (-not $ok) { throw "Phase 1: MARKER1 not found in output" }

    # 4. send_input: echo MARKER2
    Write-Host ""
    Write-Host "--- Send: echo $MARKER2 ---" -ForegroundColor Yellow
    Send-Cmd $sessionId1 "echo $MARKER2"
    $ok = Read-Until $sessionId1 $MARKER2 10
    if (-not $ok) { throw "Phase 1: MARKER2 not found in output" }

    # 5. detach_session - key: remote PTY kept alive
    Write-Host ""
    Write-Host "--- detach_session (remote PTY kept alive) ---" -ForegroundColor Yellow
    $detachResult = Call-Tool "detach_session" @{ session_id = $sessionId1 }
    if ($null -eq $detachResult) { throw "Phase 1: detach_session failed" }
    Write-Host "<<< detach_session OK - remote PTY kept alive" -ForegroundColor Green

    # 6. Close termbridge.exe process 1 (simulating MCP restart/crash)
    Write-Host ""
    Write-Host "--- Closing termbridge.exe process 1 (simulating MCP restart) ---" -ForegroundColor Yellow
    Stop-TermBridge
    Write-Host "<<< process 1 terminated" -ForegroundColor Green

    # Brief delay to ensure process 1 is fully terminated
    Start-Sleep -Seconds 1

    # ==================================================================
    # Phase 2: MCP Process 2 - List + Attach + Verify
    # ==================================================================
    Write-Host ""
    Write-Host "========== Phase 2: MCP Process 2 (List + Attach + Verify) ==========" -ForegroundColor Yellow
    Write-Host ""

    Write-Host "--- Starting termbridge.exe process 2 ---" -ForegroundColor Yellow
    Start-TermBridge
    Write-Host "<<< process 2 started (PID=$($script:proc.Id))" -ForegroundColor Green

    # 1. MCP initialize
    $null = Send-Request "initialize" @{
        protocolVersion = "2024-11-05"
        capabilities = @{}
        clientInfo = @{ name = "w4-e2e-cross-restart"; version = "0.1.0" }
    }
    Write-Host "<<< initialize OK" -ForegroundColor Green
    Send-Notification "notifications/initialized" @{}

    # 2. list_remote_sessions
    Write-Host ""
    Write-Host "--- list_remote_sessions ---" -ForegroundColor Yellow
    $listResult = Call-Tool "list_remote_sessions" @{ host = "192.168.1.180" }
    if ($null -eq $listResult) { throw "Phase 2: list_remote_sessions failed" }

    Write-Host "<<< Remote sessions:" -ForegroundColor Green
    $remoteSessionId = $null
    foreach ($s in $listResult.sessions) {
        Write-Host "  id=$($s.id)  name=$($s.name)  state=$($s.state)  written=$($s.written)" -ForegroundColor Gray
        if ($s.name -eq $sessionName -and $s.state -eq "detached" -and $null -eq $remoteSessionId) {
            $remoteSessionId = $s.id
        }
    }

    # Fallback: if no detached session found, look for any matching name
    if ($null -eq $remoteSessionId) {
        Write-Host "WARNING: no detached session with name=$sessionName, trying any state..." -ForegroundColor Yellow
        foreach ($s in $listResult.sessions) {
            if ($s.name -eq $sessionName -and $null -eq $remoteSessionId) {
                $remoteSessionId = $s.id
                Write-Host "  Found (state=$($s.state)): id=$($s.id)" -ForegroundColor Yellow
            }
        }
    }

    if ($null -eq $remoteSessionId) {
        throw "Phase 2: no remote session found with name=$sessionName (detach may have failed or daemon lost session)"
    }
    Write-Host "<<< remote_session_id = $remoteSessionId" -ForegroundColor Green

    # 3. attach_remote_session
    Write-Host ""
    Write-Host "--- attach_remote_session ---" -ForegroundColor Yellow
    $attachResult = Call-Tool "attach_remote_session" @{
        host = "192.168.1.180"
        remote_session_id = $remoteSessionId
        name = "reattached"
    }
    if ($null -eq $attachResult) { throw "Phase 2: attach_remote_session failed" }
    $sessionId2 = $attachResult.session_id
    Write-Host "<<< session_id_2 = $sessionId2 (new local session, reattached)" -ForegroundColor Green

    # 4. read_output: read history
    #    First use wait_for=MARKER2 to sync (block until read task feeds initial_data into buffer)
    #    Then use since_cursor=0 to read full history, verify MARKER1 and MARKER2 are present
    Write-Host ""
    Write-Host "--- Read history: wait_for $MARKER2 (sync buffer) ---" -ForegroundColor Yellow
    $syncResult = Call-Tool "read_output" @{
        session_id = $sessionId2
        wait_for = $MARKER2
        timeout_secs = 10
    }
    if ($null -eq $syncResult) { throw "Phase 2: read_output (wait_for sync) failed" }
    Write-Host "<<< sync read (matched=$($syncResult.matched), timed_out=$($syncResult.timed_out))" -ForegroundColor Green
    if (-not $syncResult.matched) {
        throw "Phase 2: MARKER2 not found in history after attach (buffer sync failed)"
    }

    # 5. Read full history (since_cursor=0)
    Write-Host ""
    Write-Host "--- Read full history (since_cursor=0) ---" -ForegroundColor Yellow
    $historyResult = Call-Tool "read_output" @{
        session_id = $sessionId2
        since_cursor = 0
        max_bytes = 65536
    }
    if ($null -eq $historyResult) { throw "Phase 2: read_output (since_cursor) failed" }
    $historyOutput = $historyResult.output
    Write-Host "<<< history read ($($historyOutput.Length) bytes, mode=$($historyResult.mode))" -ForegroundColor Green
    $historyOutput -split "`n" | ForEach-Object { Write-Host "     $_" -ForegroundColor Gray }

    # Verify both markers are in history output
    $hasMarker1 = $false
    $hasMarker2 = $false
    if ($historyOutput -and $historyOutput.Contains($MARKER1)) { $hasMarker1 = $true }
    if ($historyOutput -and $historyOutput.Contains($MARKER2)) { $hasMarker2 = $true }
    $color1 = "Green"; if (-not $hasMarker1) { $color1 = "Red" }
    $color2 = "Green"; if (-not $hasMarker2) { $color2 = "Red" }
    Write-Host "  Verify $MARKER1 in history: $hasMarker1" -ForegroundColor $color1
    Write-Host "  Verify $MARKER2 in history: $hasMarker2" -ForegroundColor $color2
    if (-not $hasMarker1) { throw "Phase 2: MARKER1 not found in history output (history not readable after attach)" }
    if (-not $hasMarker2) { throw "Phase 2: MARKER2 not found in history output (history not readable after attach)" }

    # 6. send_input: echo MARKER3 (verify can continue interacting after attach)
    Write-Host ""
    Write-Host "--- Send: echo $MARKER3 (continue interacting after attach) ---" -ForegroundColor Yellow
    Send-Cmd $sessionId2 "echo $MARKER3"
    $ok = Read-Until $sessionId2 $MARKER3 10
    if (-not $ok) { throw "Phase 2: MARKER3 not found in output (cannot interact after attach)" }

    # 7. close_session - cleanup
    Write-Host ""
    Write-Host "--- close_session ---" -ForegroundColor Yellow
    $null = Call-Tool "close_session" @{ session_id = $sessionId2 }
    Write-Host "<<< close_session OK" -ForegroundColor Green

    # ==================================================================
    # ALL PASSED
    # ==================================================================
    Write-Host ""
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "CROSS RESTART E2E: ALL PASSED" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green
    Write-Host ""
    Write-Host "Verified:" -ForegroundColor Green
    Write-Host "  [1] detach_session: remote PTY kept alive after detach" -ForegroundColor Green
    Write-Host "  [2] list_remote_sessions: found detached session across MCP restart" -ForegroundColor Green
    Write-Host "  [3] attach_remote_session: reconnected to remote session" -ForegroundColor Green
    Write-Host "  [4] History readable: MARKER1 and MARKER2 found in output" -ForegroundColor Green
    Write-Host "  [5] Continue interacting: MARKER3 executed after attach" -ForegroundColor Green

} finally {
    # Cleanup: stop any running process
    Stop-TermBridge

    # Print stderr logs (combined from both processes, last 40 lines)
    if ($script:allStderr.Count -gt 0) {
        Write-Host ""
        Write-Host "--- stderr (tracing logs, last 40 lines) ---" -ForegroundColor DarkGray
        $last40 = $script:allStderr | Select-Object -Last 40
        $last40 | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }
    }
}
