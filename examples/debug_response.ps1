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

$null = Send-Request 1 "initialize" @{ protocolVersion = "2024-11-05"; capabilities = @{}; clientInfo = @{ name = "debug"; version = "0.1.0" } }
Send-Notification "notifications/initialized" @{}
$r = Send-Request 2 "tools/call" @{ name = "open_session"; arguments = @{ host = "192.0.2.171" } }
Write-Host "RAW RESPONSE:" -ForegroundColor Yellow
Write-Host $r -ForegroundColor White
Write-Host ""
Write-Host "PARSED:" -ForegroundColor Yellow
$parsed = $r | ConvertFrom-Json
Write-Host "result.isError: $($parsed.result.isError)"
Write-Host "result.structuredContent:" $parsed.result.structuredContent
Write-Host "result.content[0].text:" $parsed.result.content[0].text
$parsed.result.content | ForEach-Object { Write-Host "content type: $($_.type) text: $($_.text)" }

# also test read_output structure
Start-Sleep -Seconds 1
$r2 = Send-Request 3 "tools/call" @{ name = "read_output"; arguments = @{ session_id = "sess_0"; timeout_secs = 2 } }
Write-Host ""
Write-Host "READ_OUTPUT RAW:" -ForegroundColor Yellow
Write-Host $r2 -ForegroundColor White
try { $proc.Kill() } catch {}
