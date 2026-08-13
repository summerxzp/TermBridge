#!/usr/bin/env python3
"""ADR-0017 e2e 驱动:通过 MCP stdio (newline JSON-RPC) 调工具。

用法:
  RUST_LOG=info python e2e_mcp.py <tool_name> '<json_args>'

示例:
  python e2e_mcp.py open_session '{"host": "192.168.1.180"}'
  python e2e_mcp.py list_hosts '{}'
"""
import json
import os
import subprocess
import sys

SERVER = ["target/debug/termbridge-mcp.exe"]
READ_TIMEOUT = 120  # password dialog 可能让 open_session 阻塞较久


def rpc(proc, msg):
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()


def read_response(proc):
    while True:
        line = proc.stdout.readline()
        if not line:
            raise RuntimeError("MCP server exited (EOF)")
        msg = json.loads(line)
        if msg.get("id") is not None:
            return msg


def main():
    tool, args = sys.argv[1], json.loads(sys.argv[2])
    env = dict(os.environ)
    env.setdefault("RUST_LOG", "info")
    proc = subprocess.Popen(
        SERVER,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=None,  # 继承 stderr：RUST_LOG 日志直接可见，便于排障
        text=True,
        bufsize=1,
        env=env,
    )
    rpc(proc, {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "e2e-driver", "version": "0.1"},
        },
    })
    init = read_response(proc)
    assert "result" in init, f"initialize failed: {init}"
    rpc(proc, {"jsonrpc": "2.0", "method": "notifications/initialized"})

    rpc(proc, {
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": tool, "arguments": args},
    })
    resp = read_response(proc)
    result = resp.get("result", resp)
    print(json.dumps(result, ensure_ascii=False, indent=2))
    proc.terminate()


if __name__ == "__main__":
    main()
