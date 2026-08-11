import { useState } from "react";
import type { HostEntry } from "../api";

interface Props {
  hosts: HostEntry[];
  selectedHost: string | null;
  onSelect: (host: string) => void;
  onConnect: (host: string) => Promise<void>;
  connecting: boolean;
}

export function HostList({ hosts, selectedHost, onSelect, onConnect, connecting }: Props) {
  const [connectingHost, setConnectingHost] = useState<string | null>(null);

  const handleConnect = (host: string) => {
    setConnectingHost(host);
    onConnect(host).finally(() => setConnectingHost(null));
  };

  return (
    <div className="panel">
      <div className="panel-header">
        <h2>Hosts</h2>
        <button
          className="refresh-btn"
          onClick={() => window.location.reload()}
          title="Refresh"
        >
          ↻
        </button>
      </div>
      <div className="host-list">
        {hosts.length === 0 ? (
          <div className="empty">No hosts in ~/.ssh/config</div>
        ) : (
          hosts.map((h) => (
            <div
              key={h.alias}
              className={`host-item ${selectedHost === h.alias ? "selected" : ""}`}
              onClick={() => onSelect(h.alias)}
            >
              <div className="host-info">
                <span className="host-alias">{h.alias}</span>
                <span className="host-name">{h.hostname}</span>
              </div>
              <button
                className="connect-btn"
                disabled={connecting}
                onClick={(e) => {
                  e.stopPropagation();
                  handleConnect(h.alias);
                }}
              >
                {connectingHost === h.alias ? "..." : "Connect"}
              </button>
            </div>
          ))
        )}
      </div>
    </div>
  );
}
