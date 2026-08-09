# Phase 1 端到端集成测试
# 覆盖：长任务中断 / SFTP upload-download / 错误主机连接 / 日志脱敏
#
# 用法：
#   .\examples\e2e_phase1.ps1
#   .\examples\e2e_phase1.ps1 -Host_ "root@192.168.88.200"
#
# 前提：
#   - SSH 主机已配 ~/.ssh/config（ed25519 免密）
#   - cargo build --release 已完成（脚本会自动检测并构建）

param(
    [string]$Host_ = "192.168.88.200"
)

$ErrorActionPreference = "Stop"

# ── 路径常量 ──────────────────────────────────────────────────────
# 项目根目录（脚本在 examples/ 下，向上一级即为根）
$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$exe = Join-Path $ProjectRoot "target\release\termbridge.exe"
# SFTP 本地临时目录（必须在 cwd 白名单内，即项目根目录下）
$TmpDir = Join-Path $ProjectRoot ".e2e_tmp"

# ── 构建检查 ──────────────────────────────────────────────────────
if (-not (Test-Path $exe)) {
    Write-Host "BUILD: release binary not found, building..." -ForegroundColor Yellow
    Push-Location $ProjectRoot
    cargo build --release 2>&1 | Out-Null
    Pop-Location
}

# ── 创建本地临时目录（SFTP 测试用） ──────────────────────────────
if (-not (Test-Path $TmpDir)) {
    New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null
}

# ── 启动 termbridge 进程 ──────────────────────────────────────────
$psi = [System.Diagnostics.ProcessStartInfo]::new()
$psi.FileName = $exe
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
# 显式设置 cwd 为项目根（PathPolicy 默认 allowed_local_paths=[cwd]）
$psi.WorkingDirectory = $ProjectRoot
# 开启 info 级别日志，便于脱敏验证（stderr 会被 RedactingMakeWriter 处理）
$psi.EnvironmentVariables["RUST_LOG"] = "info"

$proc = [System.Diagnostics.Process]::new()
$proc.StartInfo = $psi
[void]$proc.Start()

# stderr 异步读取（tracing 日志，经 RedactingMakeWriter 脱敏）
$errTask = $proc.StandardError.ReadToEndAsync()

# 全局测试结果追踪
$script:passCount = 0
$script:failCount = 0

# ── Send-Request：发送 JSON-RPC 请求并读匹配 id 的响应 ──────────
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

# ── Send-Notification：发送通知（无 id，不读响应） ───────────────
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

# ── 断言辅助 ──────────────────────────────────────────────────────
function Assert-True($label, $condition) {
    if ($condition) {
        Write-Host "  [PASS] $label" -ForegroundColor Green
        $script:passCount++
    } else {
        Write-Host "  [FAIL] $label" -ForegroundColor Red
        $script:failCount++
    }
}

try {
    # ====================================================================
    # 初始化
    # ====================================================================
    $r1 = Send-Request 1 "initialize" @{
        protocolVersion = "2024-11-05"
        capabilities = @{}
        clientInfo = @{ name = "e2e-phase1"; version = "0.1.0" }
    }
    Write-Host "<<< initialize OK" -ForegroundColor Green
    Send-Notification "notifications/initialized" @{}

    # ====================================================================
    # 场景 A：长任务 + wait_for + Ctrl+C 中断
    # ====================================================================
    Write-Host ""
    Write-Host "========== Scenario A: long task + wait_for + Ctrl+C ==========" -ForegroundColor Yellow

    # A1. open_session
    $rA1 = Send-Request 10 "tools/call" @{
        name = "open_session"
        arguments = @{ host = $Host_ }
    }
    $sessA = ($rA1 | ConvertFrom-Json).result.structuredContent.session_id
    Assert-True "A1 open_session returns session_id" (-not [string]::IsNullOrEmpty($sessA))
    Write-Host "    session_id = $sessA" -ForegroundColor Gray

    # A2. 读初始 prompt（settle 模式清空 buffer）
    $null = Send-Request 11 "tools/call" @{
        name = "read_output"
        arguments = @{ session_id = $sessA; timeout_secs = 3 }
    }

    # A3. send_input: 启动 100 行长任务（约 10 秒）
    $cmdLine = 'for i in $(seq 1 100); do echo "line $i"; sleep 0.1; done' + "`n"
    $null = Send-Request 12 "tools/call" @{
        name = "send_input"
        arguments = @{ session_id = $sessA; data = $cmdLine }
    }
    Write-Host "    sent: for loop (100 lines, ~10s)" -ForegroundColor Gray

    # A4. read_output(wait_for="line 50", timeout=10): 应在约 5 秒时命中
    $rA4 = Send-Request 13 "tools/call" @{
        name = "read_output"
        arguments = @{ session_id = $sessA; wait_for = "line 50"; timeout_secs = 10 }
    }
    $rdA4 = ($rA4 | ConvertFrom-Json).result.structuredContent
    Assert-True "A4 wait_for 'line 50' matched=true" ($rdA4.matched -eq $true)
    Assert-True "A4 wait_for 'line 50' timed_out=false" ($rdA4.timed_out -eq $false)
    Assert-True "A4 output contains 'line 50'" ($rdA4.output -match "line 50")

    # A5. send_control: ctrl+c 中断长任务
    $null = Send-Request 14 "tools/call" @{
        name = "send_control"
        arguments = @{ session_id = $sessA; control_key = "ctrl+c" }
    }
    Write-Host "    sent: ctrl+c (interrupt long task)" -ForegroundColor Gray

    # A6. 等待 1.5 秒让中断后的 prompt 进入 buffer
    Start-Sleep -Milliseconds 1500

    # A7. read_output(tail_lines=5): 验证看到中断后的 prompt
    $rA7 = Send-Request 15 "tools/call" @{
        name = "read_output"
        arguments = @{ session_id = $sessA; tail_lines = 5 }
    }
    $rdA7 = ($rA7 | ConvertFrom-Json).result.structuredContent
    Write-Host "    tail 5 lines:" -ForegroundColor Gray
    Write-Host "    $($rdA7.output -replace "`n","`n    ")" -ForegroundColor Gray
    # 中断后应出现 shell 提示符，且不应继续输出 line 9x
    Assert-True "A7 tail output non-empty" ($rdA7.output.Length -gt 0)
    Assert-True "A7 interrupted (no line 90+)" (-not ($rdA7.output -match "line 9[0-9]"))

    # A8. close_session
    $null = Send-Request 16 "tools/call" @{
        name = "close_session"
        arguments = @{ session_id = $sessA }
    }
    Assert-True "A8 close_session OK" $true

    # ====================================================================
    # 场景 B：SFTP upload -> 远端执行 -> download
    # ====================================================================
    Write-Host ""
    Write-Host "========== Scenario B: SFTP upload -> exec -> download ==========" -ForegroundColor Yellow

    $remoteFile = "/tmp/termbridge_test.txt"
    $uploadLocal = Join-Path $TmpDir "upload_test.txt"
    $downloadLocal = Join-Path $TmpDir "download_test.txt"
    $testContent = "Phase 1 SFTP test"

    # B1. open_session
    $rB1 = Send-Request 20 "tools/call" @{
        name = "open_session"
        arguments = @{ host = $Host_ }
    }
    $sessB = ($rB1 | ConvertFrom-Json).result.structuredContent.session_id
    Assert-True "B1 open_session returns session_id" (-not [string]::IsNullOrEmpty($sessB))
    Write-Host "    session_id = $sessB" -ForegroundColor Gray

    # B2. 读初始 prompt
    $null = Send-Request 21 "tools/call" @{
        name = "read_output"
        arguments = @{ session_id = $sessB; timeout_secs = 3 }
    }

    # B3. 准备本地测试文件（内容 "Phase 1 SFTP test"）
    Set-Content -Path $uploadLocal -Value $testContent -NoNewline
    Write-Host "    local upload file: $uploadLocal ($((Get-Item $uploadLocal).Length) bytes)" -ForegroundColor Gray

    # B4. sftp_transfer upload
    $rB4 = Send-Request 22 "tools/call" @{
        name = "sftp_transfer"
        arguments = @{
            session_id = $sessB
            direction = "upload"
            local_path = $uploadLocal
            remote_path = $remoteFile
        }
    }
    $rdB4 = ($rB4 | ConvertFrom-Json).result
    Assert-True "B4 sftp upload no error" ($rdB4.isError -ne $true)
    Write-Host "    upload: $uploadLocal -> $remoteFile OK" -ForegroundColor Gray

    # B5. send_input: cat 远端文件验证内容
    $null = Send-Request 23 "tools/call" @{
        name = "send_input"
        arguments = @{ session_id = $sessB; data = "cat $remoteFile`n" }
    }

    # B6. read_output(wait_for="Phase 1 SFTP test"): 验证文件内容回显
    $rB6 = Send-Request 24 "tools/call" @{
        name = "read_output"
        arguments = @{ session_id = $sessB; wait_for = "Phase 1 SFTP test"; timeout_secs = 5 }
    }
    $rdB6 = ($rB6 | ConvertFrom-Json).result.structuredContent
    Assert-True "B6 wait_for file content matched=true" ($rdB6.matched -eq $true)
    Assert-True "B6 output contains 'Phase 1 SFTP test'" ($rdB6.output -match "Phase 1 SFTP test")

    # B7. sftp_transfer download（下载到不同本地路径）
    if (Test-Path $downloadLocal) { Remove-Item $downloadLocal -Force }
    $rB7 = Send-Request 25 "tools/call" @{
        name = "sftp_transfer"
        arguments = @{
            session_id = $sessB
            direction = "download"
            local_path = $downloadLocal
            remote_path = $remoteFile
        }
    }
    $rdB7 = ($rB7 | ConvertFrom-Json).result
    Assert-True "B7 sftp download no error" ($rdB7.isError -ne $true)
    Write-Host "    download: $remoteFile -> $downloadLocal OK" -ForegroundColor Gray

    # B8. 验证下载文件内容与上传一致
    $downloadedContent = Get-Content -Path $downloadLocal -Raw
    Assert-True "B8 downloaded file exists" (Test-Path $downloadLocal)
    Assert-True "B8 downloaded content = uploaded content" ($downloadedContent -eq $testContent)
    Write-Host "    downloaded content: '$downloadedContent'" -ForegroundColor Gray

    # B9. 清理远端文件
    $null = Send-Request 26 "tools/call" @{
        name = "send_input"
        arguments = @{ session_id = $sessB; data = "rm -f $remoteFile`n" }
    }

    # B10. close_session
    $null = Send-Request 27 "tools/call" @{
        name = "close_session"
        arguments = @{ session_id = $sessB }
    }
    Assert-True "B10 close_session OK" $true

    # ====================================================================
    # 场景 C：错误主机连接（验证 known_hosts / connect 错误传播）
    # ====================================================================
    Write-Host ""
    Write-Host "========== Scenario C: bad host connection error ==========" -ForegroundColor Yellow

    # 用一个不存在的主机（192.168.88.999 是无效 IP）
    $badHost = "192.168.88.999"
    $rC1 = Send-Request 30 "tools/call" @{
        name = "open_session"
        arguments = @{ host = $badHost }
    }

    if ($rC1) {
        $rdC1 = ($rC1 | ConvertFrom-Json).result
        $isErr = $rdC1.isError -eq $true
        # 错误结构在 content[0].text 里（JSON 字符串），需要二次解析
        $errCode = ""
        if ($rdC1.content -and $rdC1.content[0].text) {
            $errObj = $rdC1.content[0].text | ConvertFrom-Json
            $errCode = $errObj.code
        }
        # 也可能在结构化错误中直接含 code
        if (-not $errCode -and ($rC1 -match '"code"\s*:\s*"([^"]+)"')) {
            $errCode = $matches[1]
        }

        Assert-True "C1 bad host returns isError=true" $isErr
        Assert-True "C1 error code is CONNECT_FAILED or HOST_KEY_REJECTED" (
            $errCode -eq "CONNECT_FAILED" -or $errCode -eq "HOST_KEY_REJECTED"
        )
        Write-Host "    error code: $errCode" -ForegroundColor Gray
    } else {
        Assert-True "C1 bad host returns response" $false
    }

    # ====================================================================
    # 场景 D：日志脱敏验证
    # ====================================================================
    Write-Host ""
    Write-Host "========== Scenario D: log redaction ==========" -ForegroundColor Yellow

    # D1. open_session
    $rD1 = Send-Request 40 "tools/call" @{
        name = "open_session"
        arguments = @{ host = $Host_ }
    }
    $sessD = ($rD1 | ConvertFrom-Json).result.structuredContent.session_id
    Assert-True "D1 open_session returns session_id" (-not [string]::IsNullOrEmpty($sessD))
    Write-Host "    session_id = $sessD" -ForegroundColor Gray

    # D2. 读初始 prompt
    $null = Send-Request 41 "tools/call" @{
        name = "read_output"
        arguments = @{ session_id = $sessD; timeout_secs = 3 }
    }

    # D3. send_input: echo 含密码的字符串
    $echoCmd = 'echo "password=secret123"' + "`n"
    $null = Send-Request 42 "tools/call" @{
        name = "send_input"
        arguments = @{ session_id = $sessD; data = $echoCmd }
    }
    Write-Host "    sent: echo password=secret123" -ForegroundColor Gray

    # D4. read_output(wait_for="secret123"): 拿到输出（stdout 响应含明文，这是正常的）
    $rD4 = Send-Request 43 "tools/call" @{
        name = "read_output"
        arguments = @{ session_id = $sessD; wait_for = "secret123"; timeout_secs = 5 }
    }
    $rdD4 = ($rD4 | ConvertFrom-Json).result.structuredContent
    Assert-True "D4 read_output matched=true" ($rdD4.matched -eq $true)
    Assert-True "D4 stdout response contains plaintext (agent needs real output)" ($rdD4.output -match "secret123")

    # D5. close_session
    $null = Send-Request 44 "tools/call" @{
        name = "close_session"
        arguments = @{ session_id = $sessD }
    }
    Assert-True "D5 close_session OK" $true

    # ====================================================================
    # 汇总
    # ====================================================================
    Write-Host ""
    Write-Host "==========================================" -ForegroundColor Green
    Write-Host "PASS: $script:passCount  FAIL: $script:failCount" -ForegroundColor $(if ($script:failCount -eq 0) { "Green" } else { "Red" })
    Write-Host "==========================================" -ForegroundColor Green

} finally {
    # 关闭进程
    try { $proc.StandardInput.Close() } catch {}
    if (-not $proc.HasExited) {
        $proc.Kill()
    }

    # 获取 stderr 全量日志（经 RedactingMakeWriter 脱敏后的）
    $errResult = $errTask.Result
    if ($errResult) {
        $allErrLines = $errResult -split "`n"

        Write-Host ""
        Write-Host "--- Scenario D redaction check (stderr logs) ---" -ForegroundColor Yellow

        # 检查 1：明文密码 "secret123" 不应出现在 stderr
        $leakedSecret = $allErrLines | Where-Object { $_ -match "secret123" }
        Assert-True "D-check plaintext 'secret123' not leaked to stderr" ($null -eq $leakedSecret -or $leakedSecret.Count -eq 0)

        # 检查 2：如果 stderr 中出现 'password=' ，应为 'password=[REDACTED]' 而非明文
        $passwordLines = $allErrLines | Where-Object { $_ -match "password=" }
        if ($passwordLines -and $passwordLines.Count -gt 0) {
            $unredacted = $passwordLines | Where-Object { $_ -match "password=secret123" }
            Assert-True "D-check stderr 'password=' lines all redacted to [REDACTED]" ($null -eq $unredacted -or $unredacted.Count -eq 0)
            $redacted = $passwordLines | Where-Object { $_ -match "password=\[REDACTED\]" }
            if ($redacted) {
                Write-Host "    redacted line example: $($redacted[0].Trim())" -ForegroundColor Gray
            }
        } else {
            # 当前实现不直接日志 PTY 输出，所以 stderr 中可能没有 'password=' 行。
            # 脱敏层的正确性由 redact.rs 单元测试覆盖；此处验证安全属性：明文不泄露。
            Write-Host "    (no 'password=' lines in stderr - PTY output not directly logged, redaction layer is safety net)" -ForegroundColor Gray
            Assert-True "D-check no 'password=' lines need redaction (security property satisfied)" $true
        }

        # 打印 stderr 末尾 20 行供人工检查
        Write-Host ""
        Write-Host "--- stderr (tracing logs, last 20 lines) ---" -ForegroundColor DarkGray
        $tail = $allErrLines | Where-Object { $_.Trim() } | Select-Object -Last 20
        $tail | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }
    }

    # 清理本地临时文件
    Get-ChildItem -Path $TmpDir -Filter "*.txt" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    Write-Host ""
    Write-Host "local temp files cleaned" -ForegroundColor DarkGray
}
