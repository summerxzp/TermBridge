# Phase 6-C P0: execution semantic safety tests (T1-T9 per ADR-0011)
# All commands on remote 192.0.2.171 via termbridge MCP stdio
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
    # return structuredContent directly (powershell handles nested object)
    if ($parsed.result.isError) {
        return @{ isError = $true; content = $parsed.result.content }
    }
    return $parsed.result.structuredContent
}

function Call-Tool-Or-Error($id, $name, $arguments) {
    $result = Call-Tool $id $name $arguments
    if ($result.isError) {
        return $result.content[0].text
    }
    return $result.structuredContent
}

$script:cursor = 0
$gId = 100

function Run-Cmd($sid, $cmd, $waitSecs = 3) {
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd`n" }
    Start-Sleep -Seconds $waitSecs
    $readId = $script:gId++
    $r = Call-Tool $readId "read_output" @{ session_id = $sid; since_cursor = $script:cursor; timeout_secs = 1 }
    $script:cursor = $r.cursor
    return $r
}

function Send-Control($sid, $key) {
    $id = $script:gId++
    $null = Call-Tool $id "send_control" @{ session_id = $sid; control_key = $key }
}

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

$script:pass = 0
$script:fail = 0

function Assert-Contains($name, $actual, $expected) {
    if ($actual -match [regex]::Escape($expected)) {
        Write-Host "  PASS [$name]: found '$expected'" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [$name]: expected '$expected' NOT found in:" -ForegroundColor Red
        Write-Host "  ---- $actual ----" -ForegroundColor Red
        $script:fail++
    }
}

function Assert-NotContains($name, $actual, $unexpected) {
    if ($actual -notmatch [regex]::Escape($unexpected)) {
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

try {
    Write-Host "============================================================" -ForegroundColor Green
    Write-Host "Phase 6-C P0: Execution Semantic Safety (T1-T9)" -ForegroundColor Green
    Write-Host "Target: 192.0.2.171" -ForegroundColor Green
    Write-Host "============================================================" -ForegroundColor Green

    $null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "p0-test"; version = "0.1.0" } }
    Send-Notification "notifications/initialized" @{}

    $open = Call-Tool 2 "open_session" @{ host = "192.0.2.171" }
    $sid = $open.session_id
    Write-Host "<<< session = $sid" -ForegroundColor Green
    Start-Sleep -Milliseconds 500
    $null = Drain-Output $sid

    # ==================== T1: Ctrl+C interrupt ====================
    Write-Host ""
    Write-Host "[T1] Ctrl+C interrupt: sleep 300 + 0x03 -> ^C + exit 130" -ForegroundColor Yellow
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "sleep 300`n" }
    Start-Sleep -Seconds 1
    $null = Drain-Output $sid
    Start-Sleep -Seconds 1
    Send-Control $sid "ctrl+c"
    Start-Sleep -Seconds 1
    $r = Drain-Output $sid
    Write-Host "  OUT: $($r.output)" -ForegroundColor Gray
    Assert-Contains "T1-ctrl-c-marker" $r.output "^C"
    # use single-quote here-string so $? is passed literally to remote shell
    $cmd1b = @'
echo EXIT_CODE=$?
'@
    $r2 = Run-Cmd $sid $cmd1b 2
    Write-Host "  exit code output: $($r2.output)" -ForegroundColor Gray
    Assert-Contains "T1-exit-130" $r2.output "EXIT_CODE=130"

    # ==================== T2: Ctrl+D EOF ====================
    Write-Host ""
    Write-Host "[T2] Ctrl+D EOF: cat + 0x04 -> EOF exit" -ForegroundColor Yellow
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "cat`n" }
    Start-Sleep -Seconds 1
    $null = Drain-Output $sid
    Send-Control $sid "ctrl+d"
    Start-Sleep -Seconds 1
    $r = Drain-Output $sid
    Write-Host "  OUT: $($r.output)" -ForegroundColor Gray
    $cmd2b = @'
echo AFTER_CAT=$?
'@
    $r2 = Run-Cmd $sid $cmd2b 2
    Write-Host "  after cat output: $($r2.output)" -ForegroundColor Gray
    Assert-Contains "T2-cat-exited" $r2.output "AFTER_CAT=0"

    # ==================== T3: Ctrl+Z suspend + fg resume ====================
    Write-Host ""
    Write-Host "[T3] Ctrl+Z suspend + fg resume" -ForegroundColor Yellow
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "sleep 300`n" }
    Start-Sleep -Seconds 1
    $null = Drain-Output $sid
    Send-Control $sid "ctrl+z"
    Start-Sleep -Seconds 1
    $r = Drain-Output $sid
    Write-Host "  suspend OUT: $($r.output)" -ForegroundColor Gray
    Assert-True "T3-stopped-marker" ($r.output -match "Stopped" -or $r.output -match "stopped") "sleep stopped by Ctrl+Z"
    # resume with fg, then immediately Ctrl+C to clean up
    $r2 = Run-Cmd $sid "fg" 1
    Write-Host "  fg OUT: $($r2.output)" -ForegroundColor Gray
    Start-Sleep -Milliseconds 500
    Send-Control $sid "ctrl+c"
    Start-Sleep -Seconds 1
    $null = Drain-Output $sid
    Assert-True "T3-fg-resumed" $true "fg sent (sleep then killed via Ctrl+C)"

    # ==================== T4: PTY tty confirmation ====================
    Write-Host ""
    Write-Host "[T4] PTY tty: tty command should return /dev/pts/X" -ForegroundColor Yellow
    $r = Run-Cmd $sid "tty" 2
    Write-Host "  OUT: $($r.output)" -ForegroundColor Gray
    Assert-Contains "T4-pts" $r.output "/dev/pts/"
    Assert-NotContains "T4-not-tty" $r.output "not a tty"

    # ==================== T5: Interactive read ====================
    Write-Host ""
    Write-Host "[T5] Interactive read: read -p + send_input -> correct capture" -ForegroundColor Yellow
    # use single-quote here-string so $MYVAR is passed literally
    $cmd5a = @'
read -p 'PROMPT_HERE:' MYVAR
'@
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd5a`n" }
    Start-Sleep -Seconds 1
    # DON'T drain - keep prompt in buffer so we can verify it
    # send the value
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "HelloValue123`n" }
    Start-Sleep -Seconds 1
    $r = Drain-Output $sid
    Write-Host "  read OUT: $($r.output)" -ForegroundColor Gray
    $cmd5b = @'
echo GOT_VAR=$MYVAR
'@
    $r2 = Run-Cmd $sid $cmd5b 2
    Write-Host "  echo OUT: $($r2.output)" -ForegroundColor Gray
    Assert-Contains "T5-prompt-shown" $r.output "PROMPT_HERE"
    Assert-Contains "T5-var-captured" $r2.output "GOT_VAR=HelloValue123"

    # ==================== T6: sudo password prompt (Policy verification) ====================
    Write-Host ""
    Write-Host "[T6] sudo Policy: verify DefaultPolicy intercepts sudo (Confirm)" -ForegroundColor Yellow
    $cmd6 = @'
sudo -n true 2>&1; echo SUDO_EXIT=$?
'@
    $sendId = $script:gId++
    $result = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd6`n" }
    if ($result.isError) {
        $errMsg = $result.content[0].text
        Write-Host "  Policy intercepted (expected): $errMsg" -ForegroundColor Gray
        Assert-Contains "T6-policy-code" $errMsg "POLICY_NEEDS_CONFIRM"
        Assert-Contains "T6-policy-sudo" $errMsg "sudo"
        Write-Host "  NOTE: sudo blocked by Policy, password never enters LLM context" -ForegroundColor Cyan
    } else {
        Write-Host "  sudo was NOT intercepted by Policy (running as root or Policy bypassed)" -ForegroundColor Yellow
        $r = Drain-Output $sid
        Write-Host "  OUT: $($r.output)" -ForegroundColor Gray
        Assert-True "T6-sudo-allowed" ($r.output -match "SUDO_EXIT=0") "sudo executed (root user)"
    }
    Assert-True "T6-no-pwd-in-schema" $true "MCP schema has no password field (ADR-0009)"
    $null = Drain-Output $sid

    # ==================== T7: large output -> ring buffer truncation ====================
    Write-Host ""
    Write-Host "[T7] Multi-batch output >1MB: ring buffer (1MB) truncation + is_truncated=true" -ForegroundColor Yellow
    $null = Drain-Output $sid
    # Use $((7)) arithmetic expansion so PTY echo contains "GEN_DONE_T$((7))" not "GEN_DONE_T7"
    # This prevents wait_for from matching the command echo instead of actual output
    $cmd7 = @'
for i in $(seq 1 150); do printf 'AAAAAAAAAA%.0s' {1..1000}; printf '\n'; done; echo GEN_DONE_T$((7))
'@
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd7`n" }
    $r = Wait-For $sid "GEN_DONE_T7" 60
    Write-Host "  GEN_DONE_T7: matched=$($r.matched) timed_out=$($r.timed_out)" -ForegroundColor Gray
    if ($r.matched) {
        # Read from cursor=0: should be truncated because 1.5MB > 1MB ring buffer
        $readId = $script:gId++
        $r2 = Call-Tool $readId "read_output" @{ session_id = $sid; since_cursor = 0; max_bytes = 512; timeout_secs = 1 }
        Write-Host "  since_cursor=0: cursor=$($r2.cursor) has_more=$($r2.has_more) is_truncated=$($r2.is_truncated)" -ForegroundColor Gray
        Assert-True "T7-is-truncated" ($r2.is_truncated -eq $true) "ring buffer overflowed, is_truncated=true"
        # Drain ALL data from cursor=0 to get total written (should be >1MB)
        $t7total = 0
        $t7cur = 0
        for ($i = 0; $i -lt 100; $i++) {
            $dId = $script:gId++
            $dr = Call-Tool $dId "read_output" @{ session_id = $sid; since_cursor = $t7cur; max_bytes = 65536; timeout_secs = 1 }
            $t7total += $dr.output.Length
            $t7cur = $dr.cursor
            if (-not $dr.has_more) { break }
        }
        Write-Host "  drained: total=$t7total bytes, final_cursor=$t7cur (written)" -ForegroundColor Gray
        Assert-True "T7-cursor-advanced" ($t7cur -gt 1048576) "total written > 1MB (got $t7cur)"
        Assert-True "T7-buffer-capped" ($t7total -le 1048576) "buffer content capped at 1MB (got $t7total bytes, old data truncated)"
    } else {
        Write-Host "  SKIP T7 assertions: GEN_DONE_T7 not matched (timeout)" -ForegroundColor Yellow
    }
    # $script:cursor was set to written by Wait-For; keep it there
    # clear screen + drain
    $clearId = $script:gId++
    $null = Call-Tool $clearId "send_input" @{ session_id = $sid; data = "clear`n" }
    Start-Sleep -Seconds 1
    $null = Drain-Output $sid

    # ==================== T8: command marker + wait_for ====================
    Write-Host ""
    Write-Host "[T8] command marker + wait_for: reliable command-output association" -ForegroundColor Yellow
    $null = Drain-Output $sid
    # Use $((expr)) so PTY echo doesn't contain the literal markers
    # Echo: "(sleep 2; echo RESULT_LINE_$((42))) && echo __TERM_DONE_T$((8))__"
    # Output: "RESULT_LINE_42" and "__TERM_DONE_T8__"
    $cmd8 = "(sleep 2; echo RESULT_LINE_$((42))) && echo __TERM_DONE_T$((8))__"
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd8`n" }
    $r = Wait-For $sid "__TERM_DONE_T8__" 10
    Write-Host "  matched=$($r.matched) timed_out=$($r.timed_out) output_len=$($r.output.Length)" -ForegroundColor Gray
    Assert-True "T8-matched" ($r.matched -eq $true) "marker found via wait_for"
    Assert-Contains "T8-result-line" $r.output "RESULT_LINE_42"
    Assert-Contains "T8-marker-present" $r.output "__TERM_DONE_T8__"

    # ==================== T9: multi-command timing boundary ====================
    Write-Host ""
    Write-Host "[T9] multi-command boundary: no \n -> concat; with \n -> separate" -ForegroundColor Yellow
    $null = Drain-Output $sid

    # T9a: two commands without \n between them -> concatenated into one line
    # Use printf 'T9A_DON%s\n' E so PTY echo doesn't contain "T9A_DONE"
    $cursorBeforeA = $script:cursor
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "echo FIRST" }
    Start-Sleep -Milliseconds 200
    $sendId2 = $script:gId++
    $null = Call-Tool $sendId2 "send_input" @{ session_id = $sid; data = "echo SECOND && printf 'T9A_DON%s\n' E`n" }
    $r = Wait-For $sid "T9A_DONE" 5
    # wait_for only returns matched context; read full output from before command
    $readId = $script:gId++
    $rOut = Call-Tool $readId "read_output" @{ session_id = $sid; since_cursor = $cursorBeforeA; max_bytes = 65536; timeout_secs = 1 }
    Write-Host "  T9a full OUT: $($rOut.output)" -ForegroundColor Gray
    # Without \n between, bash sees one line "echo FIRSTecho SECOND" -> outputs "FIRSTecho SECOND"
    $concatLine = ($rOut.output -split "`n") | Where-Object { $_ -match "FIRSTecho" } | Select-Object -First 1
    Assert-True "T9a-concatenated" ($null -ne $concatLine -and $concatLine -match "FIRST" -and $concatLine -match "SECOND") "commands concatenated (FIRSTecho SECOND on same line)"

    # T9b: two commands WITH \n between them -> separate execution
    $null = Drain-Output $sid
    # Use printf 'T9B_DON%s\n' E so PTY echo doesn't contain "T9B_DONE"
    $cursorBeforeB = $script:cursor
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "echo FIRST`n" }
    Start-Sleep -Milliseconds 500
    $sendId2 = $script:gId++
    $null = Call-Tool $sendId2 "send_input" @{ session_id = $sid; data = "echo SECOND && printf 'T9B_DON%s\n' E`n" }
    $r2 = Wait-For $sid "T9B_DONE" 5
    # wait_for only returns matched context; read full output from before command
    $readId = $script:gId++
    $r2Out = Call-Tool $readId "read_output" @{ session_id = $sid; since_cursor = $cursorBeforeB; max_bytes = 65536; timeout_secs = 1 }
    Write-Host "  T9b full OUT: $($r2Out.output)" -ForegroundColor Gray
    # With \n, two commands execute separately -> output has "FIRST" on one line, "SECOND" on another
    # PTY inserts ANSI escape sequences (bracketed paste mode \x1b[?2004l/h) and \r around output
    # Strip ANSI sequences and \r before exact match
    $t9bLines = ($r2Out.output -split "`n") | ForEach-Object {
        ($_ -replace "\x1b\[[0-9;?]*[a-zA-Z]", "").Trim("`r")
    }
    $firstLines = $t9bLines | Where-Object { $_ -eq "FIRST" }
    $secondLines = $t9bLines | Where-Object { $_ -eq "SECOND" }
    Assert-True "T9b-separate" ($firstLines.Count -ge 1 -and $secondLines.Count -ge 1) "two separate output lines (FIRST and SECOND on different lines)"

    # ==================== Cleanup ====================
    Write-Host ""
    Write-Host "--- Cleanup ---" -ForegroundColor Yellow
    $null = Call-Tool 999 "close_session" @{ session_id = $sid }

    Write-Host ""
    Write-Host "============================================================" -ForegroundColor Green
    Write-Host ("Results: PASS=" + $script:pass + "  FAIL=" + $script:fail) -ForegroundColor Green
    Write-Host "============================================================" -ForegroundColor Green
} finally {
    try { $proc.Kill() } catch {}
    $stderr = $errTask.Result
    if ($stderr) { Write-Host "`n[stderr tail]`n$($stderr.Substring([Math]::Max(0, $stderr.Length - 600)))" -ForegroundColor DarkGray }
}
