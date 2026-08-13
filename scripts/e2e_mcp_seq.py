#!/usr/bin/env python3
"""ADR-0017 e2e 顺序驱动:一次会话内按序调多个 MCP 工具。

用法(子命令依次执行,按行读 stdin 上的 tool 调用):
  python e2e_mcp_seq.py < tool_script.txt

tool_script.txt 每行: <tool_name> <json_args> [timeout_secs]
"""
import json
import os
import queue
import subprocess
import sys
import threading

SERVER = ["target/debug/termbridge-mcp.exe"]


class McpClient:
    def __init__(self):
        env = dict(os.environ)
        env.setdefault("RUST_LOG", "warn")
        self.proc = subprocess.Popen(
            SERVER,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
            env=env,
        )
        self.q = queue.Queue()
        self._id = 0
        threading.Thread(target=self._reader, daemon=True).start()
        self._init()

    def _reader(self):
        for line in self.proc.stdout:
            self.q.put(line)

    def _rpc(self, msg):
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def _init(self):
        self._id += 1
        self._rpc({
            "jsonrpc": "2.0",
            "id": self._id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "e2e-seq", "version": "0.1"},
            },
        })
        while True:
            m = json.loads(self.q.get(timeout=30))
            if m.get("id") == self._id:
                break
        self._rpc({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def call(self, name, args, timeout=90):
        self._id += 1
        self._rpc({
            "jsonrpc": "2.0",
            "id": self._id,
            "method": "tools/call",
            "params": {"name": name, "arguments": args},
        })
        while True:
            try:
                line = self.q.get(timeout=timeout)
            except queue.Empty:
                print(f"  [TIMEOUT {timeout}s waiting {name}]")
                return None
            m = json.loads(line)
            if m.get("id") == self._id:
                return m.get("result")

    def close(self):
        self.proc.terminate()


def main():
    client = McpClient()
    for line in sys.stdin:
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.strip().split(" ", 1)
        name = parts[0]
        args = json.loads(parts[1]) if len(parts) > 1 else {}
        timeout = 90
        print(f">>> {name} {parts[1] if len(parts) > 1 else '{}'}")
        result = client.call(name, args, timeout)
        if result is None:
            continue
        # 只打印业务内容(structuredContent / text),跳过 MCP 包装
        sc = result.get("structuredContent")
        if sc is not None:
            print("   ", json.dumps(sc, ensure_ascii=False))
        else:
            for c in result.get("content", []):
                print("   ", c.get("text", ""))
    client.close()


if __name__ == "__main__":
    main()
