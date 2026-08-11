import { useEffect, useRef } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { api, strToBase64, onPtyData, onPtyEof } from "../api";
import type { UnlistenFn } from "@tauri-apps/api/event";

interface Props {
  sessionId: string;
  host: string;
  onDisconnect: () => void;
}

export function TerminalView({ sessionId, host, onDisconnect }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const onDisconnectRef = useRef(onDisconnect);
  onDisconnectRef.current = onDisconnect;

  useEffect(() => {
    if (!containerRef.current) return;

    // 1. 创建 xterm.js 终端
    const term = new Terminal({
      fontSize: 14,
      fontFamily: "Consolas, 'Courier New', monospace",
      cursorBlink: true,
      theme: {
        background: "#1e1e2e",
        foreground: "#cdd6f4",
        cursor: "#f5e0dc",
        selectionBackground: "#585b70",
      },
    });
    termRef.current = term;

    const fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
    term.open(containerRef.current);
    fitAddon.fit();

    // 2. 同步初始尺寸到远端 PTY
    api.resize(sessionId, term.cols, term.rows).catch(console.error);

    // 3. 终端输入 → writeRaw（base64 编码）
    const inputDisposable = term.onData((data) => {
      api.writeRaw(sessionId, strToBase64(data)).catch(console.error);
    });

    // 4. 终端 resize → 同步远端 PTY
    const resizeDisposable = term.onResize(({ cols, rows }) => {
      api.resize(sessionId, cols, rows).catch(console.error);
    });

    // 5. 容器 resize → fitAddon.fit()（触发 onResize）
    const resizeObserver = new ResizeObserver(() => {
      fitAddon.fit();
    });
    resizeObserver.observe(containerRef.current);

    // 6. 注册 pty_data / pty_eof listener
    let unlistenData: UnlistenFn | null = null;
    let unlistenEof: UnlistenFn | null = null;
    let disposed = false;

    Promise.all([
      onPtyData(sessionId, (bytes) => {
        if (!disposed) term.write(bytes);
      }),
      onPtyEof(sessionId, () => {
        if (!disposed) {
          term.write("\r\n\x1b[33m[Connection closed]\x1b[0m\r\n");
          onDisconnectRef.current();
        }
      }),
    ]).then(([un1, un2]) => {
      unlistenData = un1;
      unlistenEof = un2;
      // 7. listener 就绪后启动 read loop
      api.startReadLoop(sessionId).catch(console.error);
    });

    // 8. 清理
    return () => {
      disposed = true;
      inputDisposable.dispose();
      resizeDisposable.dispose();
      resizeObserver.disconnect();
      unlistenData?.();
      unlistenEof?.();
      term.dispose();
      termRef.current = null;
    };
  }, [sessionId]);

  const handleDisconnect = () => {
    onDisconnect();
  };

  const handleCtrlC = () => {
    api.sendControl(sessionId, "ctrl_c").catch(console.error);
  };

  return (
    <div className="terminal-view">
      <div className="terminal-toolbar">
        <span className="terminal-host">{host}</span>
        <span className="terminal-session">{sessionId.slice(0, 8)}</span>
        <div className="terminal-actions">
          <button className="toolbar-btn" onClick={handleCtrlC} title="Ctrl+C">
            ^C
          </button>
          <button className="toolbar-btn disconnect" onClick={handleDisconnect}>
            Disconnect
          </button>
        </div>
      </div>
      <div className="terminal-container" ref={containerRef} />
    </div>
  );
}
