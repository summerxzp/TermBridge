# Minimal test: sftp_mkdir then sftp_transfer_dir
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

try {
    $null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "test"; version = "0.1.0" } }
    Send-Notification "notifications/initialized" @{}

    $open = Call-Tool 2 "open_session" @{ host = "192.168.1.180" }
    $sid = $open.session_id

    # Test 1: sftp_mkdir single level
    Write-Host "--- sftp_mkdir /tmp/phase5_mkdir_test ---" -ForegroundColor Yellow
    $null = Call-Tool 3 "sftp_mkdir" @{ session_id = $sid; remote_path = "/tmp/phase5_mkdir_test"; mode = "493" }
    Write-Host "<<< sftp_mkdir OK" -ForegroundColor Green

    # Test 2: sftp_mkdir nested (should fail - no recursion)
    Write-Host "--- sftp_mkdir /tmp/phase5_mkdir_test/nested/deep (expect fail) ---" -ForegroundColor Yellow
    try {
        $null = Call-Tool 4 "sftp_mkdir" @{ session_id = $sid; remote_path = "/tmp/phase5_mkdir_test/nested/deep"; mode = "493" }
        Write-Host "<<< sftp_mkdir nested OK (unexpected)" -ForegroundColor Green
    } catch {
        Write-Host "<<< sftp_mkdir nested failed (expected): $_" -ForegroundColor Yellow
    }

    # Test 3: cleanup
    $null = Call-Tool 5 "sftp_remove" @{ session_id = $sid; remote_path = "/tmp/phase5_mkdir_test"; recursive = $true }
    Write-Host "<<< cleanup OK" -ForegroundColor Green

    $null = Call-Tool 6 "close_session" @{ session_id = $sid }
    Write-Host "ALL DONE" -ForegroundColor Green

} finally {
    if ($proc.StandardInput) { $proc.StandardInput.Close() }
    if (-not $proc.HasExited) { $proc.Kill(); $proc.WaitForExit(3000) | Out-Null }
    $errResult = ""
    try { $errResult = $errTask.Wait(2000).Result } catch {}
    if ($errResult) {
        $errLines = ($errResult -split "`n") | Where-Object { $_.Trim() } | Select-Object -Last 20
        Write-Host "--- stderr (last 20 lines) ---" -ForegroundColor DarkGray
        $errLines | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }
    }
}
