# Phase 0-A 验证 1：用 JSON-RPC 消息测试 p0_echo_mcp
# 依次发送 initialize / notifications/initialized / tools/list / tools/call

$exe = "E:\Code\TermBridge\target\debug\examples\p0_echo_mcp.exe"

$messages = @(
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}'
    '{"jsonrpc":"2.0","method":"notifications/initialized"}'
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hello from termbridge"}}}'
) -join "`n"

Write-Host "=== Sending MCP JSON-RPC messages ===" -ForegroundColor Cyan
Write-Host $messages
Write-Host ""
Write-Host "=== Server output ===" -ForegroundColor Cyan

$messages | & $exe 2>$null

Write-Host ""
Write-Host "=== Done ===" -ForegroundColor Green
