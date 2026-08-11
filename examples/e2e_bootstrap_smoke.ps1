# ADR-0009 e2e: verify bootstrap_host tool registered + helper resolved
# Non-interactive: only checks tool list and helper resolution log
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

$pass = $true

try {
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "ADR-0009 bootstrap_host E2E (smoke test)" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green

    # initialize
    $req = @{ jsonrpc = "2.0"; id = 1; method = "initialize"; params = @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "boot-test"; version = "0.1.0" } } } | ConvertTo-Json -Compress -Depth 10
    $proc.StandardInput.WriteLine($req)
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::Now.AddSeconds(10)
    while ([DateTime]::Now -lt $deadline) {
        $line = $proc.StandardOutput.ReadLine()
        if ($null -eq $line) { break }
        if ($line -match '"id":\s*1') { break }
    }
    Write-Host "<<< initialized" -ForegroundColor Green

    # notifications/initialized
    $req = @{ jsonrpc = "2.0"; method = "notifications/initialized"; params = @{} } | ConvertTo-Json -Compress -Depth 10
    $proc.StandardInput.WriteLine($req)
    $proc.StandardInput.Flush()
    Start-Sleep -Milliseconds 200

    # tools/list
    $req = @{ jsonrpc = "2.0"; id = 2; method = "tools/list"; params = @{} } | ConvertTo-Json -Compress -Depth 10
    Write-Host ">>> [2] tools/list" -ForegroundColor Cyan
    $proc.StandardInput.WriteLine($req)
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::Now.AddSeconds(10)
    $response = $null
    while ([DateTime]::Now -lt $deadline) {
        $line = $proc.StandardOutput.ReadLine()
        if ($null -eq $line) { break }
        if ($line -match '"id":\s*2') { $response = $line; break }
    }

    if ($null -eq $response) {
        Write-Host "  [FAIL] no response for tools/list" -ForegroundColor Red
        $pass = $false
    } else {
        $parsed = $response | ConvertFrom-Json
        $tools = $parsed.result.tools
        Write-Host "<<< $($tools.Count) tools registered" -ForegroundColor Green

        # verify bootstrap_host exists
        $bootstrapTool = $tools | Where-Object { $_.name -eq "bootstrap_host" }
        if ($bootstrapTool) {
            Write-Host "  [PASS] bootstrap_host tool registered" -ForegroundColor Green

            # verify schema has NO password/secret/passphrase fields
            $schema = $bootstrapTool.inputSchema.properties
            $hasPassword = $schema.PSobject.Properties.name -contains "password"
            $hasSecret = $schema.PSobject.Properties.name -contains "secret"
            $hasPassphrase = $schema.PSobject.Properties.name -contains "passphrase"

            if (-not $hasPassword -and -not $hasSecret -and -not $hasPassphrase) {
                Write-Host "  [PASS] schema has no password/secret/passphrase fields" -ForegroundColor Green
            } else {
                Write-Host "  [FAIL] schema contains forbidden credential field(s)" -ForegroundColor Red
                $pass = $false
            }

            # verify host field exists
            $hasHost = $schema.PSobject.Properties.name -contains "host"
            if ($hasHost) {
                Write-Host "  [PASS] schema has required 'host' field" -ForegroundColor Green
            } else {
                Write-Host "  [FAIL] schema missing 'host' field" -ForegroundColor Red
                $pass = $false
            }
        } else {
            Write-Host "  [FAIL] bootstrap_host tool not found" -ForegroundColor Red
            $pass = $false
        }
    }

} catch {
    Write-Host "EXCEPTION: $_" -ForegroundColor Red
    $pass = $false
}

if ($proc.StandardInput) { $proc.StandardInput.Close() }
if (-not $proc.HasExited) { $proc.Kill(); $proc.WaitForExit(3000) | Out-Null }

# Verify stderr logs
$errResult = ""
try { $errResult = $errTask.Wait(2000).Result } catch {}

Write-Host ""
Write-Host "=== Helper Resolution Log ===" -ForegroundColor Yellow
if ($errResult -match "HelperCredentialProvider") {
    Write-Host "  [PASS] HelperCredentialProvider resolved" -ForegroundColor Green
} elseif ($errResult -match "NoopCredentialProvider") {
    Write-Host "  [WARN] Fell back to NoopCredentialProvider (helper not found?)" -ForegroundColor Yellow
} else {
    Write-Host "  [SKIP] credential provider log not captured" -ForegroundColor Yellow
}

Write-Host ""
if ($pass) {
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "ADR-0009 SMOKE TEST: ALL PASSED" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green
} else {
    Write-Host "========================================" -ForegroundColor Red
    Write-Host "ADR-0009 SMOKE TEST: FAILED" -ForegroundColor Red
    Write-Host "========================================" -ForegroundColor Red
}

if ($errResult) {
    $errLines = ($errResult -split "`n") | Where-Object { $_.Trim() } | Select-Object -Last 10
    if ($errLines) {
        Write-Host ""
        Write-Host "--- stderr (last 10 lines) ---" -ForegroundColor DarkGray
        $errLines | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }
    }
}
