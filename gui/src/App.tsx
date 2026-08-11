import { useState, useEffect, useCallback } from "react";
import { HostList } from "./components/HostList";
import { SessionList } from "./components/SessionList";
import { TerminalView } from "./components/TerminalView";
import { api, type HostEntry, type SessionInfo } from "./api";

export default function App() {
  const [hosts, setHosts] = useState<HostEntry[]>([]);
  const [selectedHost, setSelectedHost] = useState<string | null>(null);
  const [remoteSessions, setRemoteSessions] = useState<SessionInfo[]>([]);
  const [activeSessionId, setActiveSessionId] = useState<string | null>(null);
  const [activeHost, setActiveHost] = useState<string | null>(null);
  const [connecting, setConnecting] = useState(false);
  const [attaching, setAttaching] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // 加载主机列表
  useEffect(() => {
    api.listHosts().then(setHosts).catch((e) => setError(String(e)));
  }, []);

  // 选中主机时加载远端 session 列表
  const refreshSessions = useCallback((host: string) => {
    api
      .listRemoteSessions(host)
      .then(setRemoteSessions)
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    if (selectedHost) {
      refreshSessions(selectedHost);
    } else {
      setRemoteSessions([]);
    }
  }, [selectedHost, refreshSessions]);

  // 连接主机
  const handleConnect = async (host: string) => {
    setConnecting(true);
    setError(null);
    try {
      // 初始尺寸 80x24，TerminalView 挂载后会 fit + resize
      const sid = await api.openSession(host, 80, 24);
      setActiveSessionId(sid);
      setActiveHost(host);
    } catch (e) {
      setError(String(e));
    } finally {
      setConnecting(false);
    }
  };

  // attach 远端 session
  const handleAttach = async (remoteSessionId: string) => {
    if (!selectedHost) return;
    setAttaching(true);
    setError(null);
    try {
      const sid = await api.attachSession(selectedHost, remoteSessionId);
      setActiveSessionId(sid);
      setActiveHost(selectedHost);
    } catch (e) {
      setError(String(e));
    } finally {
      setAttaching(false);
    }
  };

  // 断开连接（detach 保留远端 session）
  const handleDisconnect = useCallback(() => {
    if (!activeSessionId) return;
    api
      .detachSession(activeSessionId)
      .catch(console.error)
      .finally(() => {
        setActiveSessionId(null);
        setActiveHost(null);
        // 刷新远端 session 列表
        if (selectedHost) refreshSessions(selectedHost);
      });
  }, [activeSessionId, selectedHost, refreshSessions]);

  return (
    <div className="app">
      <div className="sidebar">
        <HostList
          hosts={hosts}
          selectedHost={selectedHost}
          onSelect={setSelectedHost}
          onConnect={handleConnect}
          connecting={connecting}
        />
        <SessionList
          sessions={remoteSessions}
          selectedHost={selectedHost}
          onAttach={handleAttach}
          attaching={attaching}
        />
        {error && <div className="error-bar">{error}</div>}
      </div>
      <div className="main">
        {activeSessionId && activeHost ? (
          <TerminalView
            sessionId={activeSessionId}
            host={activeHost}
            onDisconnect={handleDisconnect}
          />
        ) : (
          <div className="placeholder">
            <div className="placeholder-text">
              Select a host and click Connect, or attach to an existing session.
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
