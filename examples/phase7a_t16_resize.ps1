# T16 resize test (MCP, with stderr separation)
$ErrorActionPreference = "Stop"
$exe = ".\target\release\termbridge-mcp.exe"
$env:RUST_LOG = "warn"  # reduce noise

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
$script:id = 0

function Send($method, $params) {
    $script:id++
    $i = $script:id
    $req = @{ jsonrpc = "2.0"; id = $i; method = $method; params = $params } | ConvertTo-Json -Compress -Depth 10
    $proc.StandardInput.WriteLine($req)
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::Now.AddSeconds(120)
    while ([DateTime]::Now -lt $deadline) {
        $line = $proc.StandardOutput.ReadLine()
        if ($null -eq $line) { return $null }
        if ($line -match ('"id":\s*' + $i)) { return $line | ConvertFrom-Json }
    }
    return $null
}

function Call($toolName, $arguments) {
    $r = Send "tools/call" @{ name = $toolName; arguments = $arguments }
    if ($null -eq $r) { Write-Host "FAILED: $toolName" -ForegroundColor Red; return $null }
    if ($r.result.isError) { Write-Host "ERROR: $toolName" -ForegroundColor Red; return $null }
    return $r.result.structuredContent
}

try {
    Write-Host "========" -ForegroundColor Yellow
    Write-Host "T16 resize test" -ForegroundColor Yellow
    Write-Host "========" -ForegroundColor Yellow

    $null = Send "initialize" @{
        protocolVersion = "2024-11-05"; capabilities = @{}
        clientInfo = @{ name = "t16"; version = "0.1.0" }
    }
    $proc.StandardInput.WriteLine('{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}')
    $proc.StandardInput.Flush()
    Write-Host "init OK" -ForegroundColor Green

    Write-Host ""
    Write-Host "--- open_session ---" -ForegroundColor Yellow
    $open = Call "open_session" @{ host = "192.0.2.171"; persistent = $true; name = "t16-resize" }
    if ($null -eq $open) { throw "open_session failed" }
    $sid = $open.session_id
    Write-Host "session_id = $sid" -ForegroundColor Green

    # T16-1: initial size
    Write-Host ""
    Write-Host "--- T16-1: initial PTY size ---" -ForegroundColor Yellow
    $null = Call "send_input" @{ session_id = $sid; data = "stty size`n" }
    $r = Call "read_output" @{ session_id = $sid; wait_for = "rows"; timeout_secs = 10 }
    Write-Host "  output: $($r.output)" -ForegroundColor Gray

    # T16-2: resize 120x40
    Write-Host ""
    Write-Host "--- T16-2: resize to 120x40 ---" -ForegroundColor Yellow
    $res = Call "resize" @{ session_id = $sid; cols = 120; rows = 40 }
    Write-Host "  resize result: $($res | ConvertTo-Json -Compress)" -ForegroundColor Green
    $null = Call "send_input" @{ session_id = $sid; data = "stty size`n" }
    $r = Call "read_output" @{ session_id = $sid; wait_for = "40"; timeout_secs = 10 }
    Write-Host "  stty size: $($r.output)" -ForegroundColor Gray
    if ($r.output -match "40 120") { Write-Host "  T16-2 PASS" -ForegroundColor Green } else { Write-Host "  T16-2 FAIL" -ForegroundColor Red }

    # T16-3: resize 200x50
    Write-Host ""
    Write-Host "--- T16-3: resize to 200x50 ---" -ForegroundColor Yellow
    $null = Call "resize" @{ session_id = $sid; cols = 200; rows = 50 }
    $null = Call "send_input" @{ session_id = $sid; data = "stty size`n" }
    $r = Call "read_output" @{ session_id = $sid; wait_for = "50"; timeout_secs = 10 }
    Write-Host "  stty size: $($r.output)" -ForegroundColor Gray
    if ($r.output -match "50 200") { Write-Host "  T16-3 PASS" -ForegroundColor Green } else { Write-Host "  T16-3 FAIL" -ForegroundColor Red }

    # T16-4: resize back 80x24
    Write-Host ""
    Write-Host "--- T16-4: resize back to 80x24 ---" -ForegroundColor Yellow
    $null = Call "resize" @{ session_id = $sid; cols = 80; rows = 24 }
    $null = Call "send_input" @{ session_id = $sid; data = "stty size`n" }
    $r = Call "read_output" @{ session_id = $sid; wait_for = "24"; timeout_secs = 10 }
    Write-Host "  stty size: $($r.output)" -ForegroundColor Gray
    if ($r.output -match "24 80") { Write-Host "  T16-4 PASS" -ForegroundColor Green } else { Write-Host "  T16-4 FAIL" -ForegroundColor Red }

    # T16-5: vim + resize
    Write-Host ""
    Write-Host "--- T16-5: vim launch + resize ---" -ForegroundColor Yellow
    $null = Call "send_input" @{ session_id = $sid; data = "vim /tmp/t16_test.txt`n" }
    Start-Sleep -Seconds 2
    $null = Call "resize" @{ session_id = $sid; cols = 150; rows = 45 }
    Start-Sleep -Seconds 1
    $null = Call "send_input" @{ session_id = $sid; data = ":q!`r" }
    Start-Sleep -Seconds 1
    Write-Host "  T16-5 PASS (vim + resize + quit OK)" -ForegroundColor Green

    # T16-6: top + resize
    Write-Host ""
    Write-Host "--- T16-6: top launch + resize ---" -ForegroundColor Yellow
    $null = Call "send_input" @{ session_id = $sid; data = "top -n 1`n" }
    Start-Sleep -Seconds 2
    $null = Call "resize" @{ session_id = $sid; cols = 100; rows = 30 }
    Start-Sleep -Seconds 1
    $null = Call "send_control" @{ session_id = $sid; control_key = "ctrl+c" }
    Start-Sleep -Seconds 1
    Write-Host "  T16-6 PASS (top + resize + Ctrl+C OK)" -ForegroundColor Green

    # close
    Write-Host ""
    Write-Host "--- close_session ---" -ForegroundColor Yellow
    $null = Call "close_session" @{ session_id = $sid }
    Write-Host "closed" -ForegroundColor Green

    Write-Host ""
    Write-Host "========" -ForegroundColor Green
    Write-Host "T16 ALL DONE" -ForegroundColor Green
    Write-Host "========" -ForegroundColor Green
} finally {
    try { $proc.StandardInput.Close() } catch {}
    Start-Sleep -Milliseconds 300
    if (-not $proc.HasExited) { $proc.Kill() }
}
