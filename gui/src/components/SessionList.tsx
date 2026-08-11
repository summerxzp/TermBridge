import type { SessionInfo } from "../api";

interface Props {
  sessions: SessionInfo[];
  selectedHost: string | null;
  onAttach: (remoteSessionId: string) => void;
  attaching: boolean;
}

export function SessionList({ sessions, selectedHost, onAttach, attaching }: Props) {
  return (
    <div className="panel">
      <div className="panel-header">
        <h2>Sessions{selectedHost ? ` @ ${selectedHost}` : ""}</h2>
      </div>
      <div className="session-list">
        {!selectedHost ? (
          <div className="empty">Select a host</div>
        ) : sessions.length === 0 ? (
          <div className="empty">No remote sessions</div>
        ) : (
          sessions.map((s) => (
            <div key={s.id} className="session-item">
              <div className="session-info">
                <span className="session-id">{s.id.slice(0, 8)}</span>
                <span className="session-name">{s.name || "-"}</span>
                <span className={`session-state state-${s.state}`}>{s.state}</span>
              </div>
              <button
                className="attach-btn"
                disabled={attaching || s.state === "lost"}
                onClick={() => onAttach(s.id)}
              >
                Attach
              </button>
            </div>
          ))
        )}
      </div>
    </div>
  );
}
