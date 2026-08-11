import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// ───────────────────────────────────────────────────────────────────────────
// Types
// ───────────────────────────────────────────────────────────────────────────

export interface HostEntry {
  alias: string;
  hostname: string;
}

export interface SessionInfo {
  id: string;
  name?: string;
  state: string;
  created_at: string;
  last_activity_at: string;
  pty_size: { rows: number; cols: number };
  written: number;
}

interface PtyDataEvent {
  session_id: string;
  data: string; // base64
}

interface PtyEofEvent {
  session_id: string;
}

// ───────────────────────────────────────────────────────────────────────────
// Base64 helpers（xterm.js string ↔ base64 for Tauri IPC）
// ───────────────────────────────────────────────────────────────────────────

export function strToBase64(str: string): string {
  const bytes = new TextEncoder().encode(str);
  let binary = "";
  bytes.forEach((b) => (binary += String.fromCharCode(b)));
  return btoa(binary);
}

export function base64ToBytes(b64: string): Uint8Array {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

// ───────────────────────────────────────────────────────────────────────────
// Command wrappers
// ───────────────────────────────────────────────────────────────────────────

export const api = {
  listHosts: (): Promise<HostEntry[]> => invoke<HostEntry[]>("list_hosts"),

  listRemoteSessions: (host: string): Promise<SessionInfo[]> =>
    invoke<SessionInfo[]>("list_remote_sessions", { host }),

  openSession: (host: string, cols: number, rows: number): Promise<string> =>
    invoke<string>("open_session", { host, cols, rows }),

  attachSession: (host: string, remoteSessionId: string): Promise<string> =>
    invoke<string>("attach_session", { host, remoteSessionId }),

  startReadLoop: (sessionId: string): Promise<void> =>
    invoke<void>("start_read_loop", { sessionId }),

  detachSession: (sessionId: string): Promise<void> =>
    invoke<void>("detach_session", { sessionId }),

  closeSession: (sessionId: string): Promise<void> =>
    invoke<void>("close_session", { sessionId }),

  writeRaw: (sessionId: string, data: string): Promise<void> =>
    invoke<void>("write_raw", { sessionId, data }),

  resize: (sessionId: string, cols: number, rows: number): Promise<void> =>
    invoke<void>("resize", { sessionId, cols, rows }),

  sendControl: (sessionId: string, key: string): Promise<void> =>
    invoke<void>("send_control", { sessionId, key }),
};

// ───────────────────────────────────────────────────────────────────────────
// Event listeners
// ───────────────────────────────────────────────────────────────────────────

export async function onPtyData(
  sessionId: string,
  cb: (data: Uint8Array) => void
): Promise<UnlistenFn> {
  return listen<PtyDataEvent>("pty_data", (event) => {
    if (event.payload.session_id === sessionId) {
      cb(base64ToBytes(event.payload.data));
    }
  });
}

export async function onPtyEof(
  sessionId: string,
  cb: () => void
): Promise<UnlistenFn> {
  return listen<PtyEofEvent>("pty_eof", (event) => {
    if (event.payload.session_id === sessionId) {
      cb();
    }
  });
}
