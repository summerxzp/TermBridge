# Phase 6-C P0: Execution State / Concurrency / Recovery tests (T10-T15 per ADR-0012)
# Target: 192.168.1.171
# Tests:
#   T10 - command failure status (marker + exit code)
#   T11 - marker appears before command completes (TermBridge does not validate marker position)
#   T12 - timeout does not change session state (session stays ready, Ctrl+C can interrupt)
#   T13 - consecutive commands cursor isolation (since_cursor + reqid prevents cross-command matching)
#   T14 - concurrent waiter (MCP stdio is serial; document limitation + verify sequential non-interference)
#   T15 - disconnect mid-write (kill sshd session -> lost -> reconnect -> idempotency check)
$ErrorActionPreference = "Stop"
$exe = ".\target\release\termbridge.exe"

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
    $proc.StandardInput.WriteLine($req)
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::Now.AddSeconds(120)
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
    Write-Host ">>> [$id] $name" -ForegroundColor Cyan
    $r = Send-Request $id "tools/call" @{ name = $name; arguments = $arguments }
    if ($null -eq $r) { throw "no response for $name" }
    $parsed = $r | ConvertFrom-Json
    if ($parsed.result.isError) {
        return @{ isError = $true; content = $parsed.result.content }
    }
    return $parsed.result.structuredContent
}

$script:gId = 100
$script:cursor = 0

function Drain-Output($sid) {
    $readId = $script:gId++
    $r = Call-Tool $readId "read_output" @{ session_id = $sid; since_cursor = $script:cursor; timeout_secs = 1 }
    $script:cursor = $r.cursor
    return $r
}

function Wait-For($sid, $marker, $timeout = 30) {
    $readId = $script:gId++
    $r = Call-Tool $readId "read_output" @{ session_id = $sid; wait_for = $marker; timeout_secs = $timeout }
    $script:cursor = $r.cursor
    return $r
}

function Read-Since($sid, $cursor) {
    $readId = $script:gId++
    $r = Call-Tool $readId "read_output" @{ session_id = $sid; since_cursor = $cursor; max_bytes = 65536; timeout_secs = 2 }
    return $r
}

function Get-Current-Cursor($sid) {
    $readId = $script:gId++
    $r = Call-Tool $readId "read_output" @{ session_id = $sid; tail_lines = 1; timeout_secs = 1 }
    return $r.cursor
}

function Send-Control($sid, $key) {
    $id = $script:gId++
    $null = Call-Tool $id "send_control" @{ session_id = $sid; control_key = $key }
}

function New-ReqId() {
    $r = Get-Random -Maximum 0xFFFFF
    return $r.ToString("x5")
}

# Strip ANSI escape sequences for matching (ADR-0011 best practice #8)
function Strip-Ansi($text) {
    return ($text -replace '\x1b\[[0-9;?]*[a-zA-Z]', '')
}

$script:pass = 0
$script:fail = 0

function Assert-Contains($name, $actual, $expected) {
    $cleaned = Strip-Ansi "$actual"
    if ($cleaned -match [regex]::Escape($expected)) {
        Write-Host "  PASS [$name]: found '$expected'" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [$name]: expected '$expected' NOT found in:" -ForegroundColor Red
        Write-Host "  ---- $cleaned ----" -ForegroundColor Red
        $script:fail++
    }
}

function Assert-NotContains($name, $actual, $unexpected) {
    $cleaned = Strip-Ansi "$actual"
    if ($cleaned -notmatch [regex]::Escape($unexpected)) {
        Write-Host "  PASS [$name]: '$unexpected' correctly absent" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [$name]: '$unexpected' unexpectedly present" -ForegroundColor Red
        $script:fail++
    }
}

function Assert-True($name, $cond, $detail = "") {
    if ($cond) {
        Write-Host "  PASS [$name]: $detail" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [$name]: $detail" -ForegroundColor Red
        $script:fail++
    }
}

function Assert-Eq($name, $actual, $expected, $detail = "") {
    if ("$actual" -eq "$expected") {
        Write-Host "  PASS [$name]: $detail (got '$actual')" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [$name]: expected '$expected' got '$actual' — $detail" -ForegroundColor Red
        $script:fail++
    }
}

try {
    Write-Host "============================================================" -ForegroundColor Green
    Write-Host "Phase 6-C P0: Execution State / Concurrency / Recovery" -ForegroundColor Green
    Write-Host "Target: 192.168.1.171" -ForegroundColor Green
    Write-Host "============================================================" -ForegroundColor Green

    $null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "p0-state-test"; version = "0.1.0" } }
    Send-Notification "notifications/initialized" @{}

    $open = Call-Tool 2 "open_session" @{ host = "192.168.1.171" }
    $sid = $open.session_id
    Write-Host "<<< session = $sid" -ForegroundColor Green
    Start-Sleep -Milliseconds 500
    $null = Drain-Output $sid

    # ==================== T10: command failure status (marker + exit code) ====================
    Write-Host ""
    Write-Host "[T10] command failure status: false -> exit 1; true -> exit 0" -ForegroundColor Yellow

    # T10a: false -> exit code 1
    $reqidA = New-ReqId
    $cmdA = @'
false; printf '\n__TB_DONE__:%s:%s\n' '{0}' "$?"
'@.Trim() -f $reqidA
    $cursorBeforeA = $script:cursor
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmdA`n" }
    $r = Wait-For $sid "__TB_DONE__:${reqidA}:" 10
    Write-Host "  matched=$($r.matched) timed_out=$($r.timed_out)" -ForegroundColor Gray
    Assert-True "T10a-matched" ($r.matched -eq $true) "marker found for false command"
    $fullA = Read-Since $sid $cursorBeforeA
    Write-Host "  full output: $(Strip-Ansi $fullA.output)" -ForegroundColor Gray
    # parse exit code from marker: __TB_DONE__:<reqid>:<exit_code>
    $cleanA = Strip-Ansi $fullA.output
    if ($cleanA -match "__TB_DONE__:${reqidA}:(\d+)") {
        $exitCodeA = $matches[1]
        Assert-Eq "T10a-exit-code" $exitCodeA "1" "false returns exit code 1"
    } else {
        Assert-True "T10a-exit-code" $false "could not parse exit code from marker"
    }

    # T10b: true -> exit code 0
    $null = Drain-Output $sid
    $reqidB = New-ReqId
    $cmdB = @'
true; printf '\n__TB_DONE__:%s:%s\n' '{0}' "$?"
'@.Trim() -f $reqidB
    $cursorBeforeB = $script:cursor
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmdB`n" }
    $r = Wait-For $sid "__TB_DONE__:${reqidB}:" 10
    Assert-True "T10b-matched" ($r.matched -eq $true) "marker found for true command"
    $fullB = Read-Since $sid $cursorBeforeB
    $cleanB = Strip-Ansi $fullB.output
    if ($cleanB -match "__TB_DONE__:${reqidB}:(\d+)") {
        $exitCodeB = $matches[1]
        Assert-Eq "T10b-exit-code" $exitCodeB "0" "true returns exit code 0"
    } else {
        Assert-True "T10b-exit-code" $false "could not parse exit code from marker"
    }

    # ==================== T11: marker appears before command completes ====================
    Write-Host ""
    Write-Host "[T11] marker appears before command completes (TermBridge does not validate position)" -ForegroundColor Yellow
    Write-Host "  cmd: printf 'T%sONE\n' \$((11)); sleep 5; printf 'L%sATE\n' \$((11))" -ForegroundColor Gray
    Write-Host "  expect: wait_for matches T11ONE immediately, L11ATE not yet present" -ForegroundColor Gray

    $null = Drain-Output $sid
    # Use arithmetic expansion so PTY echo does not contain literal markers
    $cmd11 = @'
printf 'T%sONE\n' $((11)); sleep 5; printf 'L%sATE\n' $((11))
'@.Trim()
    $cursorBefore11 = $script:cursor
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd11`n" }
    $r = Wait-For $sid "T11ONE" 10
    Write-Host "  matched=$($r.matched) timed_out=$($r.timed_out)" -ForegroundColor Gray
    Assert-True "T11-immediate-match" ($r.matched -eq $true) "marker matched (appeared before command completes)"

    # At this point sleep 5 is still running; L11ATE should NOT be in buffer yet
    $partial = Read-Since $sid $cursorBefore11
    Assert-NotContains "T11-late-not-yet" $partial.output "L11ATE"

    # Wait for sleep 5 to finish, then L11ATE should appear
    Write-Host "  waiting 6s for sleep 5 + printf L11ATE..." -ForegroundColor Gray
    Start-Sleep -Seconds 6
    $full11 = Read-Since $sid $cursorBefore11
    Assert-Contains "T11-late-eventually" $full11.output "L11ATE"

    Write-Host "  NOTE: TermBridge does not validate marker position." -ForegroundColor Cyan
    Write-Host "  Completion correctness is Agent protocol responsibility." -ForegroundColor Cyan

    # ==================== T12: timeout does not change session state ====================
    Write-Host ""
    Write-Host "[T12] timeout does not change session state" -ForegroundColor Yellow
    Write-Host "  cmd: sleep 300; wait_for timeout=3 -> timed_out + session_state=ready" -ForegroundColor Gray

    $null = Drain-Output $sid
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "sleep 300`n" }
    Start-Sleep -Milliseconds 500
    $null = Drain-Output $sid

    # wait_for with short timeout — should time out, NOT match
    $r = Wait-For $sid "__NEVER_MATCH_T12__" 3
    Write-Host "  matched=$($r.matched) timed_out=$($r.timed_out)" -ForegroundColor Gray
    Assert-True "T12-timed-out" ($r.timed_out -eq $true -or $r.matched -eq $false) "wait_for timed out (sleep 300 still running)"
    Assert-True "T12-not-matched" ($r.matched -eq $false) "marker not matched"

    # Check session_state from read_output result
    $readId = $script:gId++
    $r2 = Call-Tool $readId "read_output" @{ session_id = $sid; tail_lines = 1; timeout_secs = 1 }
    Write-Host "  session_state=$($r2.session_state)" -ForegroundColor Gray
    Assert-Eq "T12-session-ready" $r2.session_state "ready" "session stays ready after timeout"

    # Ctrl+C should still be able to interrupt the running sleep
    Write-Host "  sending Ctrl+C to interrupt sleep 300..." -ForegroundColor Gray
    Send-Control $sid "ctrl+c"
    Start-Sleep -Seconds 1
    $r3 = Drain-Output $sid
    Write-Host "  after Ctrl+C: $(Strip-Ansi $r3.output)" -ForegroundColor Gray
    Assert-Contains "T12-ctrl-c-interrupt" $r3.output "^C"

    # Verify sleep was actually killed (exit code 130 = 128+SIGINT)
    $cmd12b = @'
echo EXIT_AFTER_INTERRUPT=$?
'@
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd12b`n" }
    Start-Sleep -Seconds 1
    $r4 = Drain-Output $sid
    Write-Host "  exit code: $(Strip-Ansi $r4.output)" -ForegroundColor Gray
    Assert-Contains "T12-exit-130" $r4.output "EXIT_AFTER_INTERRUPT=130"

    # ==================== T13: consecutive commands cursor isolation ====================
    Write-Host ""
    Write-Host "[T13] consecutive commands cursor isolation" -ForegroundColor Yellow
    Write-Host "  cmd A (AAA_T13) + marker; cmd B (BBB_T13) + marker" -ForegroundColor Gray
    Write-Host "  verify: output_A contains AAA not BBB; output_B contains BBB not AAA" -ForegroundColor Gray

    $null = Drain-Output $sid

    # Command A
    $reqid13A = New-ReqId
    $cmd13A = @'
echo AAA_T13; printf '\n__TB_DONE__:%s:%s\n' '{0}' "$?"
'@.Trim() -f $reqid13A
    $cursorBefore13A = $script:cursor
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd13A`n" }
    $r = Wait-For $sid "__TB_DONE__:${reqid13A}:" 10
    Assert-True "T13a-matched" ($r.matched -eq $true) "cmd A marker matched"
    $outputA = Read-Since $sid $cursorBefore13A

    # Command B
    $reqid13B = New-ReqId
    $cmd13B = @'
echo BBB_T13; printf '\n__TB_DONE__:%s:%s\n' '{0}' "$?"
'@.Trim() -f $reqid13B
    $cursorBefore13B = $script:cursor
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd13B`n" }
    $r = Wait-For $sid "__TB_DONE__:${reqid13B}:" 10
    Assert-True "T13b-matched" ($r.matched -eq $true) "cmd B marker matched"
    $outputB = Read-Since $sid $cursorBefore13B

    Write-Host "  outputA: $(Strip-Ansi $outputA.output)" -ForegroundColor Gray
    Write-Host "  outputB: $(Strip-Ansi $outputB.output)" -ForegroundColor Gray

    Assert-Contains "T13-A-has-AAA" $outputA.output "AAA_T13"
    Assert-NotContains "T13-A-no-BBB" $outputA.output "BBB_T13"
    Assert-Contains "T13-B-has-BBB" $outputB.output "BBB_T13"
    Assert-NotContains "T13-B-no-AAA" $outputB.output "AAA_T13"

    # ==================== T14: concurrent waiter (MCP stdio serial limitation) ====================
    Write-Host ""
    Write-Host "[T14] concurrent waiter (MCP stdio serial limitation)" -ForegroundColor Yellow
    Write-Host "  MCP stdio processes requests serially — true concurrency not testable here." -ForegroundColor Cyan
    Write-Host "  Contract 4 (waiter non-consumption) is covered by Rust unit tests in ADR-0003." -ForegroundColor Cyan
    Write-Host "  This test verifies sequential non-interference: two wait_for calls back-to-back." -ForegroundColor Gray

    $null = Drain-Output $sid

    # Send both commands first, then wait for each marker sequentially
    $reqid14A = New-ReqId
    $reqid14B = New-ReqId
    $cmd14A = @'
echo CONCURRENT_A_T14; printf '\n__TB_DONE__:%s:%s\n' '{0}' "$?"
'@.Trim() -f $reqid14A
    $cmd14B = @'
echo CONCURRENT_B_T14; printf '\n__TB_DONE__:%s:%s\n' '{0}' "$?"
'@.Trim() -f $reqid14B
    $cursorBefore14 = $script:cursor

    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd14A`n" }
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd14B`n" }

    # Wait for A first, then B
    $rA = Wait-For $sid "__TB_DONE__:${reqid14A}:" 10
    Assert-True "T14-wait-A" ($rA.matched -eq $true) "wait_for A matched"
    $rB = Wait-For $sid "__TB_DONE__:${reqid14B}:" 10
    Assert-True "T14-wait-B" ($rB.matched -eq $true) "wait_for B matched"

    $full14 = Read-Since $sid $cursorBefore14
    Assert-Contains "T14-has-A" $full14.output "CONCURRENT_A_T14"
    Assert-Contains "T14-has-B" $full14.output "CONCURRENT_B_T14"

    Write-Host "  NOTE: True concurrent waiter test requires multi-threaded MCP transport." -ForegroundColor Cyan
    Write-Host "  Current MCP stdio is serial; contract 4 unit-tested in output.rs." -ForegroundColor Cyan

    # ==================== T15: disconnect mid-write (kill sshd session) ====================
    Write-Host ""
    Write-Host "[T15] disconnect mid-write: kill sshd session -> lost -> reconnect -> idempotency" -ForegroundColor Yellow

    $null = Drain-Output $sid

    # Step 1: send touch + schedule pkill in background + sleep 30 (all in one command)
    # pkill must be nohup'd in background so it runs while sleep 30 blocks the shell
    Write-Host "  Step 1: send 'touch /tmp/tb_t15_test; nohup kill-in-2s &; sleep 30'" -ForegroundColor Gray
    $cmd15 = @'
touch /tmp/tb_t15_test; nohup bash -c 'sleep 2; pkill -9 -f "sshd:.*@"' >/dev/null 2>&1 & sleep 30
'@.Trim()
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd15`n" }
    Write-Host "  Step 2: waiting 4s for pkill (delay 2s) to kill sshd session..." -ForegroundColor Gray
    Start-Sleep -Seconds 4

    # Step 3: check session_state = lost
    Write-Host "  Step 3: verify session_state = lost" -ForegroundColor Gray
    $readId = $script:gId++
    $r = Call-Tool $readId "read_output" @{ session_id = $sid; tail_lines = 1; timeout_secs = 2 }
    Write-Host "  session_state=$($r.session_state)" -ForegroundColor Gray
    Assert-Eq "T15-lost" $r.session_state "lost" "session lost after sshd kill"

    # Step 4: reconnect
    Write-Host "  Step 4: reconnect_session" -ForegroundColor Gray
    $reconnId = $script:gId++
    $rc = Call-Tool $reconnId "reconnect_session" @{ session_id = $sid }
    Write-Host "  status=$($rc.status)" -ForegroundColor Gray
    Assert-Eq "T15-reconnected" $rc.status "reconnected" "reconnect succeeded"

    Start-Sleep -Milliseconds 800
    $null = Drain-Output $sid

    # Step 5: idempotency check — did the touch command execute before disconnect?
    Write-Host "  Step 5: idempotency check (test -f /tmp/tb_t15_test)" -ForegroundColor Gray
    $checkCmd = "test -f /tmp/tb_t15_test && echo FILE_EXISTS || echo FILE_MISSING"
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$checkCmd`n" }
    Start-Sleep -Seconds 1
    $r = Drain-Output $sid
    Write-Host "  check result: $(Strip-Ansi $r.output)" -ForegroundColor Gray

    $cleaned = Strip-Ansi $r.output
    $fileExists = $cleaned -match "FILE_EXISTS"
    $fileMissing = $cleaned -match "FILE_MISSING"
    Assert-True "T15-idempotency-checkable" ($fileExists -or $fileMissing) "can determine if command executed (EXISTS or MISSING)"

    Write-Host "  Result: $(if ($fileExists) { 'FILE_EXISTS — touch executed before disconnect' } else { 'FILE_MISSING — touch did not execute' })" -ForegroundColor Cyan
    Write-Host "  KEY: 'request failed' != 'command did not execute' (contract 6)" -ForegroundColor Cyan
    Write-Host "  Agent MUST use idempotency check before retrying." -ForegroundColor Cyan

    # Step 6: cleanup
    Write-Host "  Step 6: cleanup /tmp/tb_t15_test" -ForegroundColor Gray
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "rm -f /tmp/tb_t15_test`n" }
    Start-Sleep -Milliseconds 500
    $null = Drain-Output $sid

    # ==================== Summary ====================
    Write-Host ""
    Write-Host "============================================================" -ForegroundColor Green
    Write-Host "Results: $script:pass PASS / $script:fail FAIL" -ForegroundColor Green
    Write-Host "============================================================" -ForegroundColor Green

    if ($script:fail -gt 0) {
        Write-Host "SOME TESTS FAILED" -ForegroundColor Red
    }
}
catch {
    Write-Host "EXCEPTION: $_" -ForegroundColor Red
    Write-Host $_.ScriptStackTrace -ForegroundColor Red
    $script:fail++
}
finally {
    try {
        $proc.Kill()
    } catch {}
    if ($errTask.IsCompleted) {
        $errOut = $errTask.Result
        if ($errOut.Length -gt 0) {
            Write-Host "`n[stderr]" -ForegroundColor Yellow
            Write-Host $errOut -ForegroundColor Gray
        }
    }
    if ($script:fail -gt 0) { exit 1 } else { exit 0 }
}
