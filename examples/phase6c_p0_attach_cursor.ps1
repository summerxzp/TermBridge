# Phase 6-C P0: T17 Attach/Cursor Boundary Test (ADR-0012 契约 ⑦)
# Target: 192.168.1.171
#
# 验证：detach/attach 后输出连续不丢失、不重复、可继续交互
#
# 流程：
#   1. open_session(persistent=true) → 创建远端 daemon session
#   2. send_input("echo MARKER_A") → wait_for → 验证 MARKER_A 出现
#   3. send_input("nohup bash -c 'sleep 5; echo MARKER_DELAYED' &") → 启动后台任务
#   4. detach_session → 远端 PTY 保活
#   5. sleep 8 → 等待后台 MARKER_DELAYED 输出到 daemon buffer
#   6. list_remote_sessions → 验证 session 存在 + written 增长
#   7. attach_remote_session → 重新连接
#   8. read_output(wait_for=MARKER_DELAYED) → 验证 detach 期间输出不丢失
#   9. read_output(since_cursor=0) → 全量 → 验证 MARKER_A/MARKER_DELAYED 各出现一次（不重复）
#  10. send_input("echo MARKER_AFTER_ATTACH") → 验证 attach 后可继续交互
#  11. close_session → 清理

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

$script:gId = 100
$script:pass = 0
$script:fail = 0

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

function Strip-Ansi($text) {
    return ($text -replace '\x1b\[[0-9;?]*[a-zA-Z]', '')
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

function Assert-Count-Exactly($name, $actual, $expected, $count) {
    $cleaned = Strip-Ansi "$actual"
    $matches = ([regex]::Escape($expected) | ForEach-Object { [regex]::Matches($cleaned, $_) }).Count
    # 用 Select-String -AllMatches 更可靠
    $matches = ([regex]::new([regex]::Escape($expected))).Matches($cleaned).Count
    if ($matches -eq $count) {
        Write-Host "  PASS [$name]: '$expected' appears exactly $count time(s)" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [$name]: '$expected' appears $matches time(s), expected $count" -ForegroundColor Red
        $script:fail++
    }
}

# 验证独立输出行计数（排除 PTY 回显行中的 marker 子串）
# 回显行格式：`root@host:~# echo MARKER`，输出行格式：`MARKER`（独立成行）
function Assert-Output-Line-Count($name, $actual, $expected, $count) {
    $cleaned = Strip-Ansi "$actual"
    $lines = $cleaned -split "`n"
    $matches = ($lines | Where-Object { $_.Trim() -eq $expected }).Count
    if ($matches -eq $count) {
        Write-Host "  PASS [$name]: '$expected' as standalone line appears exactly $count time(s)" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "  FAIL [$name]: '$expected' as standalone line appears $matches time(s), expected $count" -ForegroundColor Red
        Write-Host "  ---- lines ----" -ForegroundColor Red
        $lines | ForEach-Object { Write-Host "    | $_" -ForegroundColor DarkRed }
        $script:fail++
    }
}

function Wait-For($sid, $marker, $timeout = 30) {
    $readId = $script:gId++
    $r = Call-Tool $readId "read_output" @{ session_id = $sid; wait_for = $marker; timeout_secs = $timeout }
    return $r
}

function Read-Since($sid, $cursor) {
    $readId = $script:gId++
    $r = Call-Tool $readId "read_output" @{ session_id = $sid; since_cursor = $cursor; max_bytes = 65536; timeout_secs = 2 }
    return $r
}

function Drain-Output($sid) {
    $readId = $script:gId++
    $r = Call-Tool $readId "read_output" @{ session_id = $sid; tail_lines = 1; timeout_secs = 1 }
    return $r
}

try {
    Write-Host "============================================================" -ForegroundColor Green
    Write-Host "Phase 6-C P0: T17 Attach/Cursor Boundary Test" -ForegroundColor Green
    Write-Host "Target: 192.168.1.171" -ForegroundColor Green
    Write-Host "Contract 7: attach 后输出精确恢复，恰好一次不重复不遗漏" -ForegroundColor Green
    Write-Host "============================================================" -ForegroundColor Green

    $null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "t17-test"; version = "0.1.0" } }
    Send-Notification "notifications/initialized" @{}

    # 生成唯一 marker
    $runId = Get-Date -Format "HHmmss"
    $MARKER_A = "T17_A_${runId}"
    $MARKER_DELAYED = "T17_D_${runId}"
    $MARKER_AFTER = "T17_X_${runId}"
    $sessionName = "t17-cursor-${runId}"

    # ==================== Step 1: open_session(persistent=true) ====================
    Write-Host ""
    Write-Host "[Step 1] open_session(persistent=true, name=$sessionName)" -ForegroundColor Yellow
    $openId = $script:gId++
    $open = Call-Tool $openId "open_session" @{ host = "192.168.1.171"; persistent = $true; name = $sessionName }
    $sid = $open.session_id
    Write-Host "<<< session_id = $sid" -ForegroundColor Green
    Start-Sleep -Milliseconds 500
    $null = Drain-Output $sid

    # ==================== Step 2: send MARKER_A ====================
    Write-Host ""
    Write-Host "[Step 2] send_input: echo $MARKER_A" -ForegroundColor Yellow
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "echo $MARKER_A`n" }
    $r = Wait-For $sid $MARKER_A 15
    Assert-True "T17-A-appeared" ($r.matched -eq $true) "MARKER_A appeared in output"
    Assert-Contains "T17-A-in-output" $r.output $MARKER_A

    # ==================== Step 3: 启动后台任务（detach 期间产生输出）====================
    # 注意：不重定向 stdout，让输出进 PTY，daemon buffer 才能捕获
    # 用 printf '\n%s\n' 确保 marker 独立成行，避免与 shell 提示符交错
    Write-Host ""
    Write-Host "[Step 3] start background task: sleep 5; printf marker" -ForegroundColor Yellow
    $bgCmd = "(sleep 5; printf '\n%s\n' '$MARKER_DELAYED') &"
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$bgCmd`n" }
    Start-Sleep -Milliseconds 800
    $null = Drain-Output $sid

    # ==================== Step 4: detach_session ====================
    Write-Host ""
    Write-Host "[Step 4] detach_session (remote PTY kept alive)" -ForegroundColor Yellow
    $detachId = $script:gId++
    $null = Call-Tool $detachId "detach_session" @{ session_id = $sid }
    Write-Host "<<< detached, waiting 8s for background task to produce MARKER_DELAYED..." -ForegroundColor Green

    # ==================== Step 5: 等待后台任务输出 ====================
    Start-Sleep -Seconds 8

    # ==================== Step 6: list_remote_sessions ====================
    Write-Host ""
    Write-Host "[Step 6] list_remote_sessions (verify session exists + written grew)" -ForegroundColor Yellow
    $listId = $script:gId++
    $list = Call-Tool $listId "list_remote_sessions" @{ host = "192.168.1.171" }

    $remoteSessionId = $null
    $remoteWritten = 0
    foreach ($s in $list.sessions) {
        Write-Host "  remote: id=$($s.id) name=$($s.name) state=$($s.state) written=$($s.written)" -ForegroundColor Gray
        if ($s.name -eq $sessionName) {
            $remoteSessionId = $s.id
            $remoteWritten = $s.written
        }
    }
    Assert-True "T17-session-found" ($null -ne $remoteSessionId) "detached session found in list_remote_sessions"
    Assert-True "T17-written-grew" ($remoteWritten -gt 0) "written > 0 (daemon produced output during detach)"

    # ==================== Step 7: attach_remote_session ====================
    Write-Host ""
    Write-Host "[Step 7] attach_remote_session (reconnect to remote session)" -ForegroundColor Yellow
    $attachId = $script:gId++
    $attach = Call-Tool $attachId "attach_remote_session" @{ host = "192.168.1.171"; remote_session_id = $remoteSessionId }
    $sid2 = $attach.session_id
    Write-Host "<<< new local session_id = $sid2" -ForegroundColor Green
    Start-Sleep -Milliseconds 500

    # ==================== Step 8: 验证 detach 期间输出不丢失 ====================
    Write-Host ""
    Write-Host "[Step 8] read_output(wait_for=$MARKER_DELAYED) — verify detach output not lost" -ForegroundColor Yellow
    $r = Wait-For $sid2 $MARKER_DELAYED 15
    Write-Host "  matched=$($r.matched) timed_out=$($r.timed_out)" -ForegroundColor Gray
    Assert-True "T17-delayed-not-lost" ($r.matched -eq $true) "MARKER_DELAYED found after attach (detach output not lost)"

    # ==================== Step 9: 全量读取，验证不重复 ====================
    Write-Host ""
    Write-Host "[Step 9] read_output(since_cursor=0) — verify no duplicates" -ForegroundColor Yellow
    $full = Read-Since $sid2 0
    $cleanedFull = Strip-Ansi $full.output
    Write-Host "  full output ($($full.output.Length) bytes):" -ForegroundColor Gray
    $cleanedFull -split "`n" | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkGray }

    # MARKER_A 作为独立输出行应恰好 1 次（排除 PTY 回显行）
    Assert-Output-Line-Count "T17-A-no-dup" $cleanedFull $MARKER_A 1
    # MARKER_DELAYED 作为独立输出行应恰好 1 次（排除 PTY 回显行）
    Assert-Output-Line-Count "T17-delayed-no-dup" $cleanedFull $MARKER_DELAYED 1

    # ==================== Step 10: 验证 attach 后可继续交互 ====================
    Write-Host ""
    Write-Host "[Step 10] send_input: echo $MARKER_AFTER (verify can interact after attach)" -ForegroundColor Yellow
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid2; data = "echo $MARKER_AFTER`n" }
    $r = Wait-For $sid2 $MARKER_AFTER 15
    Assert-True "T17-after-attach" ($r.matched -eq $true) "MARKER_AFTER appeared (can interact after attach)"

    # ==================== Step 11: 清理 ====================
    Write-Host ""
    Write-Host "[Step 11] close_session (cleanup)" -ForegroundColor Yellow
    $closeId = $script:gId++
    $null = Call-Tool $closeId "close_session" @{ session_id = $sid2 }

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
    try { $proc.Kill() } catch {}
    if ($errTask.IsCompleted) {
        $errOut = $errTask.Result
        if ($errOut.Length -gt 0) {
            Write-Host "`n[stderr] (last 20 lines)" -ForegroundColor Yellow
            ($errOut -split "`n" | Where-Object { $_.Trim() } | Select-Object -Last 20) | ForEach-Object { Write-Host "  $_" -ForegroundColor Gray }
        }
    }
    if ($script:fail -gt 0) { exit 1 } else { exit 0 }
}
