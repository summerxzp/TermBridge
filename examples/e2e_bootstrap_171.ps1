# ADR-0009 e2e: full bootstrap_host flow against 192.0.2.171
# This is INTERACTIVE: will pop up CredUI dialog for password input
# User needs to enter password "123" in the dialog
$ErrorActionPreference = "Stop"
$exe = ".\target\release\termbridge-mcp.exe"

Write-Host "========================================" -ForegroundColor Yellow
Write-Host "INTERACTIVE TEST: bootstrap_host" -ForegroundColor Yellow
Write-Host "========================================" -ForegroundColor Yellow
Write-Host "A Windows credential dialog will pop up." -ForegroundColor Yellow
Write-Host "Enter password: 123" -ForegroundColor Yellow
Write-Host "========================================" -ForegroundColor Yellow
Start-Sleep -Seconds 2

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
    # bootstrap can take a while (password dialog + SSH + deploy + verify)
    $timeoutSecs = 120
    $deadline = [DateTime]::Now.AddSeconds($timeoutSecs)
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

$pass = $true

try {
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "ADR-0009 bootstrap_host E2E (171)" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green

    $null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "boot-test"; version = "0.1.0" } }
    Send-Notification "notifications/initialized" @{}
    Write-Host "<<< initialized" -ForegroundColor Green

    Write-Host ""
    Write-Host ">>> Calling bootstrap_host for 192.0.2.171" -ForegroundColor Cyan
    Write-Host ">>> WATCH FOR CREDUI DIALOG POPUP!" -ForegroundColor Yellow
    $r = Send-Request 2 "tools/call" @{ name = "bootstrap_host"; arguments = @{ host = "192.0.2.171" } }

    if ($null -eq $r) {
        Write-Host "  [FAIL] no response (timeout 120s)" -ForegroundColor Red
        $pass = $false
    } else {
        $parsed = $r | ConvertFrom-Json
        if ($parsed.result.isError) {
            Write-Host "  [FAIL] tool returned error" -ForegroundColor Red
            $parsed.result.content | ForEach-Object { Write-Host "  ERR: $($_.text)" -ForegroundColor Red }
            $pass = $false
        } else {
            $sc = $parsed.result.structuredContent
            Write-Host "<<< status: $($sc.status)" -ForegroundColor Green
            if ($sc.host) { Write-Host "<<< host: $($sc.host)" }
            if ($sc.authentication) { Write-Host "<<< auth: $($sc.authentication)" }
            if ($sc.identity_source) { Write-Host "<<< source: $($sc.identity_source)" }
            if ($sc.reason) { Write-Host "<<< reason: $($sc.reason)" }

            switch ($sc.status) {
                "already_configured" {
                    Write-Host "  [PASS] already_configured (key auth already works)" -ForegroundColor Green
                }
                "bootstrapped" {
                    Write-Host "  [PASS] bootstrapped (public key deployed + verified)" -ForegroundColor Green
                }
                "cancelled" {
                    Write-Host "  [WARN] user cancelled password dialog" -ForegroundColor Yellow
                    $pass = $false
                }
                "authentication_failed" {
                    Write-Host "  [FAIL] password authentication failed" -ForegroundColor Red
                    $pass = $false
                }
                "bootstrap_failed" {
                    Write-Host "  [FAIL] bootstrap failed: key verification after install" -ForegroundColor Red
                    $pass = $false
                }
                default {
                    Write-Host "  [FAIL] unknown status: $($sc.status)" -ForegroundColor Red
                    $pass = $false
                }
            }
        }
    }

} catch {
    Write-Host "EXCEPTION: $_" -ForegroundColor Red
    $pass = $false
}

if ($proc.StandardInput) { $proc.StandardInput.Close() }
if (-not $proc.HasExited) { $proc.Kill(); $proc.WaitForExit(3000) | Out-Null }

# stderr logs
$errResult = ""
try { $errResult = $errTask.Wait(2000).Result } catch {}

Write-Host ""
Write-Host "=== Tracing Logs ===" -ForegroundColor Yellow
if ($errResult -match "HelperCredentialProvider") {
    Write-Host "  [PASS] HelperCredentialProvider resolved" -ForegroundColor Green
}
if ($errResult -match "password requested") {
    Write-Host "  [PASS] password requested via helper" -ForegroundColor Green
}
if ($errResult -match "password received") {
    Write-Host "  [PASS] password received from helper" -ForegroundColor Green
}
if ($errResult -match "ssh authenticated") {
    Write-Host "  [PASS] SSH authenticated" -ForegroundColor Green
}
if ($errResult -match "bootstrap") {
    Write-Host "  [INFO] bootstrap logs found" -ForegroundColor Green
}

Write-Host ""
if ($pass) {
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "ADR-0009 E2E: PASSED" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green
} else {
    Write-Host "========================================" -ForegroundColor Red
    Write-Host "ADR-0009 E2E: FAILED" -ForegroundColor Red
    Write-Host "========================================" -ForegroundColor Red
}

if ($errResult) {
    $errLines = ($errResult -split "`n") | Where-Object { $_.Trim() } | Select-Object -Last 30
    if ($errLines) {
        Write-Host ""
        Write-Host "--- stderr (last 30 lines) ---" -ForegroundColor DarkGray
        $errLines | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }
    }
}
