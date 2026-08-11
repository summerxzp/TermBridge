# Task: connect 171 - neofetch + wazuh resource query (since_cursor incremental read)
# Improvement: use since_cursor, only read new output since last read, no history
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

# global cursor: since_cursor incremental read cursor, updated after each read
$script:cursor = 0
$gId = 100

# send command + sleep + read incremental output via since_cursor
function Run-Cmd($sid, $cmd, $waitSecs = 3) {
    $sendId = $script:gId++
    $null = Call-Tool $sendId "send_input" @{ session_id = $sid; data = "$cmd`n" }
    Start-Sleep -Seconds $waitSecs
    $readId = $script:gId++
    $r = Call-Tool $readId "read_output" @{ session_id = $sid; since_cursor = $script:cursor; timeout_secs = 1 }
    $script:cursor = $r.cursor
    return $r
}

# drain existing output (consume banner after open_session, establish cursor baseline)
function Drain-Output($sid) {
    $readId = $script:gId++
    $r = Call-Tool $readId "read_output" @{ session_id = $sid; since_cursor = $script:cursor; timeout_secs = 1 }
    $script:cursor = $r.cursor
    return $r
}

try {
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "Task: neofetch + wazuh resource on 171 (since_cursor)" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green

    $null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "task171"; version = "0.1.0" } }
    Send-Notification "notifications/initialized" @{}
    Write-Host "<<< initialized" -ForegroundColor Green

    $open = Call-Tool 2 "open_session" @{ host = "192.168.1.171" }
    $sid = $open.session_id
    Write-Host "<<< session_id = $sid" -ForegroundColor Green
    Start-Sleep -Milliseconds 500
    # drain banner to establish cursor baseline (later reads won't include banner)
    $null = Drain-Output $sid

    # ===== Task 1: neofetch =====
    Write-Host ""
    Write-Host "=== Task 1: neofetch ===" -ForegroundColor Yellow
    $r = Run-Cmd $sid "which neofetch && echo NEOFETCH_INSTALLED || echo NEOFETCH_MISSING"
    Write-Host "--- output ---" -ForegroundColor Gray
    Write-Host $r.output -ForegroundColor White

    if ($r.output -match "NEOFETCH_INSTALLED") {
        Write-Host "PASS: neofetch already installed" -ForegroundColor Green
    } else {
        Write-Host "Installing neofetch..." -ForegroundColor Yellow
        $r = Run-Cmd $sid "apt-get install -y neofetch 2>&1 | tail -3" 60
        Write-Host $r.output -ForegroundColor Gray
    }

    Write-Host ""
    Write-Host "--- neofetch ---" -ForegroundColor Yellow
    $r = Run-Cmd $sid "neofetch --stdout 2>/dev/null | head -25" 5
    Write-Host $r.output -ForegroundColor White

    # ===== Task 2: wazuh 资源 =====
    Write-Host ""
    Write-Host "=== Task 2: Wazuh agent resources ===" -ForegroundColor Yellow

    Write-Host ""
    Write-Host "--- 2.1 processes ---" -ForegroundColor Yellow
    $r = Run-Cmd $sid "ps -eo pid,user,pcpu,pmem,rss,cmd --sort=-pmem | grep wazuh | grep -v grep"
    Write-Host $r.output -ForegroundColor White

    Write-Host ""
    Write-Host "--- 2.2 process count + total RSS ---" -ForegroundColor Yellow
    $cmd22 = @'
echo PROC_COUNT=$(pgrep -c -f wazuh); ps -eo rss,cmd | grep wazuh | grep -v grep | awk '{s+=1} END {print TOTAL_RSS_KB=s}'
'@
    $r = Run-Cmd $sid $cmd22
    Write-Host $r.output -ForegroundColor White

    Write-Host ""
    Write-Host "--- 2.3 service status ---" -ForegroundColor Yellow
    $r = Run-Cmd $sid "systemctl status wazuh-agent --no-pager 2>&1 | head -12"
    Write-Host $r.output -ForegroundColor White

    Write-Host ""
    Write-Host "--- 2.4 disk usage ---" -ForegroundColor Yellow
    $r = Run-Cmd $sid "du -sh /var/ossec/ 2>/dev/null; du -sh /var/ossec/logs/ 2>/dev/null; du -sh /var/ossec/queue/ 2>/dev/null"
    Write-Host $r.output -ForegroundColor White

    Write-Host ""
    Write-Host "--- 2.5 system context ---" -ForegroundColor Yellow
    $r = Run-Cmd $sid 'free -m | head -3; echo CORES=$(nproc)'
    Write-Host $r.output -ForegroundColor White

    # close
    Write-Host ""
    Write-Host "--- Cleanup ---" -ForegroundColor Yellow
    $null = Call-Tool 50 "close_session" @{ session_id = $sid }
    Write-Host "<<< closed" -ForegroundColor Gray

    Write-Host ""
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "Done" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green
} finally {
    try { $proc.Kill() } catch {}
    $stderr = $errTask.Result
    if ($stderr) { Write-Host "`n[stderr tail]`n$($stderr.Substring([Math]::Max(0, $stderr.Length - 600)))" -ForegroundColor DarkGray }
}
