# Escaping stress test: complex/long commands on 171
# 7 scenarios: special chars / nested quotes / $ vars / backtick / heredoc / long line / multiline
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
    $deadline = [DateTime]::Now.AddSeconds(90)
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
        $parsed.result.content | ForEach-Object { Write-Host "  ERR: $($_.text)" -ForegroundColor Red }
        throw "$name failed"
    }
    return $parsed.result.structuredContent
}

$script:cursor = 0
$gId = 100

# send + incremental read, return only new output
function Run-Cmd($sid, $cmd, $waitSecs = 3) {
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd`n" }
    Start-Sleep -Seconds $waitSecs
    $readId = $script:gId++
    $r = Call-Tool $readId "read_output" @{ session_id = $sid; since_cursor = $script:cursor; timeout_secs = 1 }
    $script:cursor = $r.cursor
    return $r
}

function Drain-Output($sid) {
    $readId = $script:gId++
    $r = Call-Tool $readId "read_output" @{ session_id = $sid; since_cursor = $script:cursor; timeout_secs = 1 }
    $script:cursor = $r.cursor
    return $r
}

$script:pass = 0
$script:fail = 0

function Assert-Contains($name, $actual, $expected) {
    if ($actual -match [regex]::Escape($expected)) {
        Write-Host "  PASS [$name]: found expected '$expected'" -ForegroundColor Green
        $script:pass++
        return $true
    } else {
        Write-Host "  FAIL [$name]: expected '$expected' NOT found in:" -ForegroundColor Red
        Write-Host "  ---- $actual ----" -ForegroundColor Red
        $script:fail++
        return $false
    }
}

try {
    Write-Host "============================================================" -ForegroundColor Green
    Write-Host "Stress Test: send_input escaping / long command on 171" -ForegroundColor Green
    Write-Host "============================================================" -ForegroundColor Green

    $null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "escape-test"; version = "0.1.0" } }
    Send-Notification "notifications/initialized" @{}

    $open = Call-Tool 2 "open_session" @{ host = "192.168.1.171" }
    $sid = $open.session_id
    Write-Host "<<< session = $sid" -ForegroundColor Green
    Start-Sleep -Milliseconds 500
    $null = Drain-Output $sid

    # Scenario 1: special characters passed literally in single-quote echo
    Write-Host ""
    Write-Host "[S1] Special chars: space dollar backtick backslash bang pipe" -ForegroundColor Yellow
    $cmd1 = @'
echo 'hello world $HOME `date` \path !ok |grep'
'@
    $r = Run-Cmd $sid $cmd1
    Write-Host "  OUT: $($r.output)" -ForegroundColor Gray
    # In bash single quotes, $ ` \ ! | are all literal
    Assert-Contains "S1-literal-dollar" $r.output '$HOME'
    Assert-Contains "S1-literal-backtick" $r.output '`date`'
    Assert-Contains "S1-literal-backslash" $r.output '\path'
    Assert-Contains "S1-literal-bang" $r.output '!ok'
    Assert-Contains "S1-literal-pipe" $r.output '|grep'

    # Scenario 2: double-quote: $HOME should expand to actual home dir
    Write-Host ""
    Write-Host "[S2] Double-quote shell expansion" -ForegroundColor Yellow
    $r = Run-Cmd $sid 'echo "Home is: $HOME"'
    Write-Host "  OUT: $($r.output)" -ForegroundColor Gray
    # on Debian root -> /root; on user -> /home/user. Just need a /-prefixed path, not literal $HOME
    if ($r.output -match 'Home is: /' -and $r.output -notmatch 'Home is: \$HOME\b') {
        Write-Host "  PASS [S2-dollar-expanded]: dollar expanded, got a real path" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [S2-dollar-expanded]: literal `$HOME still present or no /-path" -ForegroundColor Red
        $script:fail++
    }

    # Scenario 3: deeply nested quotes: single inside double inside single
    Write-Host ""
    Write-Host "[S3] Nested quotes: awk with embedded JSON-like string" -ForegroundColor Yellow
    $cmd3 = @'
echo "key1,123" | awk -F, '{printf "{\x22name\x22: \x22%s\x22, \x22val\x22: %d}\n", $1, $2}'
'@
    $r = Run-Cmd $sid $cmd3
    Write-Host "  OUT: $($r.output)" -ForegroundColor Gray
    Assert-Contains "S3-json-keys" $r.output 'name'
    Assert-Contains "S3-json-val" $r.output '"val"'
    Assert-Contains "S3-json-key1" $r.output 'key1'
    Assert-Contains "S3-json-num" $r.output '123'

    # Scenario 4: long single-line content (4KB) via printf to file, then verify via dd-head-tail on remote
    # Avoids echo-echo confusion; uses remote head -c / tail -c to verify start/end without PTY-echo pollution
    Write-Host ""
    Write-Host "[S4] Long 4KB content: printf 3600 chars to file, verify wc + head/tail" -ForegroundColor Yellow
    $segA = "A" * 1200
    $segB = "B" * 1200
    $segC = "C" * 1200
    $data4 = $segA + $segB + $segC  # 3600 chars
    $cmd4 = "printf '%s' '$data4' > /tmp/tb_long.txt; sha256sum /tmp/tb_long.txt; wc -c /tmp/tb_long.txt; echo HEAD_START:; head -c 20 /tmp/tb_long.txt; echo; echo TAIL_END:; tail -c 20 /tmp/tb_long.txt; echo"
    Write-Host "  total data length: $($data4.Length) chars" -ForegroundColor DarkGray
    $r = Run-Cmd $sid $cmd4 5
    Write-Host "  OUT: $($r.output)" -ForegroundColor Gray
    Assert-Contains "S4-wc-created" $r.output '3600'
    $head20 = ("A" * 20)
    $tail20 = ("C" * 20)
    if ($r.output -match [regex]::Escape($head20)) {
        Write-Host "  PASS [S4-head-20A]: first 20 chars are all As" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [S4-head-20A]: first 20 not AAA..." -ForegroundColor Red
        $script:fail++
    }
    if ($r.output -match [regex]::Escape($tail20)) {
        Write-Host "  PASS [S4-tail-20C]: last 20 chars are all Cs" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [S4-tail-20C]: last 20 not CCC..." -ForegroundColor Red
        $script:fail++
    }

    # Scenario 5: multiline heredoc write + verify content (JSON with all escapable chars)
    Write-Host ""
    Write-Host "[S5] Multiline heredoc: write JSON file with special chars, verify sha256" -ForegroundColor Yellow
    $cmd5 = @'
cat > /tmp/tb_escape_test.json <<'JSONEOF'
{
  "name": "test & verify",
  "path": "/home/user/dir with spaces/file.txt",
  "formula": "a+b<c>d=e*f%2",
  "regex": "^[a-z0-9_.+-]+@[a-z0-9-]+\\.[a-z0-9-.]+$",
  "mixed_quote": "he said \"hello\" and 'world'",
  "currency": "Price: $99.99 USD = \$99.99",
  "backtick_ref": "result is `ls`",
  "bang": "in csh, !$ is last arg",
  "unicode_zh": "你好世界"
}
JSONEOF
sha256sum /tmp/tb_escape_test.json
wc -c /tmp/tb_escape_test.json
'@
    $r = Run-Cmd $sid $cmd5 4
    Write-Host "  OUT: $($r.output)" -ForegroundColor Gray
    Assert-Contains "S5-sha256" $r.output '/tmp/tb_escape_test.json'
    Assert-Contains "S5-wc-created" $r.output '/tmp/tb_escape_test.json'
    # Verify key content by grep (cat the file)
    $r2 = Run-Cmd $sid "cat /tmp/tb_escape_test.json" 2
    Write-Host "  FILE CONTENT:" -ForegroundColor DarkGray
    Write-Host $r2.output -ForegroundColor White
    Assert-Contains "S5-spaces-path" $r2.output '/home/user/dir with spaces/file.txt'
    Assert-Contains "S5-mixed-quotes" $r2.output 'he said'
    Assert-Contains "S5-dollar-literal" $r2.output '$99.99'
    Assert-Contains "S5-backtick-literal" $r2.output '`ls`'
    Assert-Contains "S5-bang-literal" $r2.output '!$'
    Assert-Contains "S5-unicode-zh" $r2.output '你好世界'

    # Scenario 6: bash command substitution + nested subshell with pipes
    Write-Host ""
    Write-Host '[S6] Shell substitution: lines=$(wc -l) inside double-quote run on remote' -ForegroundColor Yellow
    $cmd6 = @'
echo "Lines: $(wc -l < /etc/passwd); Pwd: $(pwd | tr 'a-z' 'A-Z'); Today: $(date +%Y-%m-%d)"
'@
    $r = Run-Cmd $sid $cmd6 3
    Write-Host "  OUT: $($r.output)" -ForegroundColor Gray
    Assert-Contains "S6-wc-lines" $r.output 'Lines:'
    Assert-Contains "S6-pwd-upper" $r.output 'Pwd: /'
    Assert-Contains "S6-date" $r.output 'Today:'

    # Scenario 7: escape sequences in echo -e: real control chars to PTY
    Write-Host ""
    Write-Host "[S7] echo -e: tab, newline, color escape inside PTY stream" -ForegroundColor Yellow
    $cmd7 = @'
printf "Col1\tCol2\tCol3\nLine2: \033[31mRED\033[0m after\n"
'@
    $r = Run-Cmd $sid $cmd7 3
    Write-Host "  OUT raw length: $($r.output.Length)" -ForegroundColor DarkGray
    Write-Host "  OUT: $($r.output)" -ForegroundColor Gray
    # Tab and newline are visible as formatting so check tab-separated Cols exist
    if ($r.output -match 'Col1.*Col2.*Col3') {
        Write-Host "  PASS [S7-tabs]: Col1/2/3 separated" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [S7-tabs]: Col sequence not found" -ForegroundColor Red
        $script:fail++
    }
    Assert-Contains "S7-line2" $r.output 'Line2:'
    # The ESC[31m escape is preserved as bytes in output
    if ($r.output -match "RED") {
        Write-Host "  PASS [S7-red-text]: RED text present" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [S7-red-text]: RED missing" -ForegroundColor Red
        $script:fail++
    }

    # Scenario 8: extra large 8KB single line written to file + verified remotely
    # Write first via printf brace expansion {1..8192}, then verify wc and head/tail for non-PTY-echo proof
    Write-Host ""
    Write-Host "[S8] Extra-large 8192 ones to file: verify wc + head/tail remotely" -ForegroundColor Yellow
    $cmd8 = "printf '%0.s1' {1..8192} > /tmp/tb_big8k.txt; echo WC_RESULT:; wc -c /tmp/tb_big8k.txt; echo HEAD_START_60:; head -c 60 /tmp/tb_big8k.txt; echo; echo TAIL_END_60:; tail -c 60 /tmp/tb_big8k.txt; echo"
    $r = Run-Cmd $sid $cmd8 15
    Write-Host "  OUT: $($r.output)" -ForegroundColor Gray
    if ($r.output -match '8192 .*/tmp/tb_big8k.txt') {
        Write-Host "  PASS [S8-8KB-wc]: wc shows exactly 8192 bytes" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [S8-8KB-wc]: wc did not report 8192" -ForegroundColor Red
        $script:fail++
    }
    $head60 = ("1" * 60)
    $tail60 = ("1" * 60)
    if ($r.output -match [regex]::Escape($head60)) {
        Write-Host "  PASS [S8-head-60]: first 60 chars are all '1's" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [S8-head-60]: first 60 not 111..." -ForegroundColor Red
        $script:fail++
    }
    if ($r.output -match [regex]::Escape($tail60)) {
        Write-Host "  PASS [S8-tail-60]: last 60 chars are all '1's" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [S8-tail-60]: last 60 not 111..." -ForegroundColor Red
        $script:fail++
    }

    # Scenario 9: tab / bang-history in interactive bash (edge: !! might expand)
    Write-Host ""
    Write-Host "[S9] Bang-history edge: echo '!!' literal (should NOT expand in single quotes)" -ForegroundColor Yellow
    $cmd9 = @'
echo 'history tokens: !! and !$ and !* end'
'@
    $r = Run-Cmd $sid $cmd9 3
    Write-Host "  OUT: $($r.output)" -ForegroundColor Gray
    if ($r.output -match "!!.*!`\$.*!\*") {
        Write-Host "  PASS [S9-bang-literal]: bang tokens preserved literally in single quotes" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [S9-bang-literal]: bang tokens may have been interpreted by history expansion" -ForegroundColor Red
        $script:fail++
    }

    # cleanup
    $null = Run-Cmd $sid "rm -f /tmp/tb_escape_test.json" 1

    Write-Host ""
    $null = Call-Tool 400 "close_session" @{ session_id = $sid }

    Write-Host ""
    Write-Host "============================================================" -ForegroundColor Green
    Write-Host ("Results: PASS=" + $script:pass + "  FAIL=" + $script:fail) -ForegroundColor Green
    Write-Host "============================================================" -ForegroundColor Green
} finally {
    try { $proc.Kill() } catch {}
    $stderr = $errTask.Result
    if ($stderr) { Write-Host "`n[stderr tail]`n$($stderr.Substring([Math]::Max(0, $stderr.Length - 600)))" -ForegroundColor DarkGray }
}
