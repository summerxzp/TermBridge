# Phase 5 e2e (non-destructive)
# Verify: detect_remote_env + sftp_transfer_dir (upload test dir, then cleanup)
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
$localDownloadDir = $null

# Create local test dir for upload (must be under cwd for PathPolicy)
$localTestDir = Join-Path (Get-Location) ".phase5_test_$(Get-Date -Format 'yyyyMMddHHmmss')"
$null = New-Item -ItemType Directory -Path $localTestDir -Force
$null = New-Item -ItemType Directory -Path "$localTestDir\subdir" -Force
"hello from termbridge phase5" | Out-File "$localTestDir\file1.txt" -Encoding utf8
"nested file content" | Out-File "$localTestDir\subdir\file2.txt" -Encoding utf8

try {
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "Phase 5 Remote Workspace E2E" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green

    $null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "phase5-test"; version = "0.1.0" } }
    Send-Notification "notifications/initialized" @{}
    Write-Host "<<< initialized" -ForegroundColor Green

    $open = Call-Tool 2 "open_session" @{ host = "192.168.1.180" }
    $sid = $open.session_id
    Write-Host "<<< session_id = $sid" -ForegroundColor Green

    # 1. detect_remote_env
    Write-Host ""
    Write-Host "--- detect_remote_env ---" -ForegroundColor Yellow
    $env = Call-Tool 3 "detect_remote_env" @{ session_id = $sid }
    $envInfo = $env.env
    Write-Host "<<< OS: $($envInfo.os)" -ForegroundColor Gray
    Write-Host "<<< Shell: $($envInfo.shell)" -ForegroundColor Gray
    Write-Host "<<< PATH: $($envInfo.path.Substring(0, [Math]::Min(80, $envInfo.path.Length)))..." -ForegroundColor Gray
    Write-Host "<<< Tools:" -ForegroundColor Gray
    foreach ($tool in $envInfo.tools) {
        $status = if ($tool.installed) { "INSTALLED" } else { "missing" }
        Write-Host "    $($tool.name): $status ($($tool.path))" -ForegroundColor Gray
    }

    if ($envInfo.os -match "Linux") {
        Write-Host "  [PASS] OS detected as Linux" -ForegroundColor Green
    } else {
        Write-Host "  [FAIL] OS not detected as Linux: $($envInfo.os)" -ForegroundColor Red
        $pass = $false
    }
    if ($envInfo.shell -match "bash|zsh|sh") {
        Write-Host "  [PASS] Shell detected" -ForegroundColor Green
    } else {
        Write-Host "  [FAIL] Shell unexpected: $($envInfo.shell)" -ForegroundColor Red
        $pass = $false
    }
    $installedTools = @($envInfo.tools | Where-Object { $_.installed })
    if ($installedTools.Count -ge 1) {
        Write-Host "  [PASS] At least 1 tool installed ($($installedTools.Count) total)" -ForegroundColor Green
    } else {
        Write-Host "  [FAIL] No tools detected as installed" -ForegroundColor Red
        $pass = $false
    }

    # 2. sftp_transfer_dir (upload)
    Write-Host ""
    Write-Host "--- sftp_transfer_dir (upload) ---" -ForegroundColor Yellow
    $remoteTestDir = "/tmp/termbridge_phase5_test"
    $upResult = Call-Tool 4 "sftp_transfer_dir" @{
        session_id = $sid
        direction = "upload"
        local_path = $localTestDir
        remote_path = $remoteTestDir
    }
    Write-Host "<<< uploaded $($upResult.files_transferred) files" -ForegroundColor Green
    if ($upResult.files_transferred -eq 2) {
        Write-Host "  [PASS] 2 files uploaded" -ForegroundColor Green
    } else {
        Write-Host "  [FAIL] expected 2 files, got $($upResult.files_transferred)" -ForegroundColor Red
        $pass = $false
    }

    # 3. sftp_transfer_dir (download) to different local dir
    Write-Host ""
    Write-Host "--- sftp_transfer_dir (download) ---" -ForegroundColor Yellow
    $localDownloadDir = Join-Path (Get-Location) ".phase5_download_$(Get-Date -Format 'yyyyMMddHHmmss')"
    $dlResult = Call-Tool 5 "sftp_transfer_dir" @{
        session_id = $sid
        direction = "download"
        local_path = $localDownloadDir
        remote_path = $remoteTestDir
    }
    Write-Host "<<< downloaded $($dlResult.files_transferred) files" -ForegroundColor Green
    if ($dlResult.files_transferred -eq 2) {
        Write-Host "  [PASS] 2 files downloaded" -ForegroundColor Green
    } else {
        Write-Host "  [FAIL] expected 2 files, got $($dlResult.files_transferred)" -ForegroundColor Red
        $pass = $false
    }

    # 4. Verify downloaded content
    $dlFile1 = Join-Path $localDownloadDir "file1.txt"
    $dlFile2 = Join-Path $localDownloadDir "subdir\file2.txt"
    if ((Test-Path $dlFile1) -and (Test-Path $dlFile2)) {
        $content1 = Get-Content $dlFile1 -Raw
        $content2 = Get-Content $dlFile2 -Raw
        if ($content1 -match "hello from termbridge phase5" -and $content2 -match "nested file content") {
            Write-Host "  [PASS] Downloaded files content matches" -ForegroundColor Green
        } else {
            Write-Host "  [FAIL] Content mismatch: f1='$content1' f2='$content2'" -ForegroundColor Red
            $pass = $false
        }
    } else {
        Write-Host "  [FAIL] Downloaded files missing: f1=$(Test-Path $dlFile1) f2=$(Test-Path $dlFile2)" -ForegroundColor Red
        $pass = $false
    }

    # 5. Cleanup remote test dir
    Write-Host ""
    Write-Host "--- cleanup ---" -ForegroundColor Yellow
    try {
        $null = Call-Tool 6 "sftp_remove" @{ session_id = $sid; remote_path = $remoteTestDir; recursive = $true }
        Write-Host "<<< remote cleaned" -ForegroundColor Green
    } catch {
        Write-Host "  [WARN] remote cleanup failed (non-critical)" -ForegroundColor Yellow
    }

    # 6. close_session
    $null = Call-Tool 7 "close_session" @{ session_id = $sid }
    Write-Host "<<< closed" -ForegroundColor Green

} catch {
    Write-Host "EXCEPTION: $_" -ForegroundColor Red
    $pass = $false
} finally {
    if ($proc.StandardInput) { $proc.StandardInput.Close() }
    if (-not $proc.HasExited) { $proc.Kill(); $proc.WaitForExit(3000) | Out-Null }
    # Cleanup local test dirs
    Remove-Item -Recurse -Force $localTestDir -ErrorAction SilentlyContinue
    if ($localDownloadDir) { Remove-Item -Recurse -Force $localDownloadDir -ErrorAction SilentlyContinue }
}

Write-Host ""
if ($pass) {
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "PHASE 5 E2E: ALL PASSED" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green
} else {
    Write-Host "========================================" -ForegroundColor Red
    Write-Host "PHASE 5 E2E: FAILED" -ForegroundColor Red
    Write-Host "========================================" -ForegroundColor Red
}

$errResult = ""
try { $errResult = $errTask.Wait(2000).Result } catch {}
if ($errResult) {
    $errLines = ($errResult -split "`n") | Where-Object { $_.Trim() } | Select-Object -Last 15
    if ($errLines) {
        Write-Host ""
        Write-Host "--- stderr (last 15 lines) ---" -ForegroundColor DarkGray
        $errLines | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }
    }
}
