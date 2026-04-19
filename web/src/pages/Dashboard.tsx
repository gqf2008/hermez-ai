import { useEffect, useState } from 'react';
import type { SystemStatus, Session } from '../types';
import { api, mockApi, safeApi } from '../api/client';

function fmtSize(bytes: number): string {
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(1)} GB`;
  if (bytes >= 1e6) return `${(bytes / 1e6).toFixed(1)} MB`;
  if (bytes >= 1e3) return `${(bytes / 1e3).toFixed(1)} KB`;
  return `${bytes} B`;
}

function fmtTokens(n: number): string {
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}K`;
  return `${n}`;
}

export default function Dashboard() {
  const [status, setStatus] = useState<SystemStatus | null>(null);
  const [sessions, setSessions] = useState<Session[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    Promise.all([
      safeApi(api.getStatus, mockApi.getStatus),
      safeApi(api.getSessions, mockApi.getSessions),
    ]).then(([s, sess]) => {
      setStatus(s);
      setSessions(sess.slice(0, 5));
      setLoading(false);
    });
  }, []);

  if (loading) return <div className="loading">Loading…</div>;
  if (!status) return <div className="error">Failed to load dashboard</div>;

  return (
    <div className="page">
      <h1>Dashboard</h1>

      <div className="cards">
        <div className="card">
          <div className="card-title">Sessions</div>
          <div className="card-value">{status.sessions_total}</div>
          <div className="card-sub">{status.sessions_today} today</div>
        </div>
        <div className="card">
          <div className="card-title">Tokens</div>
          <div className="card-value">{fmtTokens(status.tokens_total)}</div>
          <div className="card-sub">total consumed</div>
        </div>
        <div className="card">
          <div className="card-title">Cron Jobs</div>
          <div className="card-value">{status.cron_active}</div>
          <div className="card-sub">/ {status.cron_jobs} configured</div>
        </div>
        <div className="card">
          <div className="card-title">Plugins</div>
          <div className="card-value">{status.plugins_active}</div>
          <div className="card-sub">/ {status.plugins} loaded</div>
        </div>
        <div className="card">
          <div className="card-title">Disk</div>
          <div className="card-value">{fmtSize(status.disk_usage_bytes)}</div>
          <div className="card-sub">~/.hermes</div>
        </div>
      </div>

      <h2>Recent Sessions</h2>
      <table className="data-table">
        <thead>
          <tr>
            <th>Title</th>
            <th>Platform</th>
            <th>Model</th>
            <th>Tokens</th>
            <th>Updated</th>
          </tr>
        </thead>
        <tbody>
          {sessions.map(s => (
            <tr key={s.id}>
              <td>{s.title}</td>
              <td><span className={`badge platform-${s.platform}`}>{s.platform}</span></td>
              <td>{s.model}</td>
              <td>{fmtTokens(s.input_tokens + s.output_tokens)}</td>
              <td>{new Date(s.updated_at).toLocaleString()}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
