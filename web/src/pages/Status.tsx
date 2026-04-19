import { useEffect, useState } from 'react';
import type { SystemStatus, CronJob } from '../types';
import { api, mockApi, safeApi } from '../api/client';

function fmtUptime(seconds: number): string {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  return `${h}h ${m}m ${s}s`;
}

export default function Status() {
  const [status, setStatus] = useState<SystemStatus | null>(null);
  const [cronJobs, setCronJobs] = useState<CronJob[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    Promise.all([
      safeApi(api.getStatus, mockApi.getStatus),
      safeApi(api.getCronJobs, mockApi.getCronJobs),
    ]).then(([s, c]) => {
      setStatus(s);
      setCronJobs(c);
      setLoading(false);
    });
  }, []);

  if (loading) return <div className="loading">Loading…</div>;
  if (!status) return <div className="error">Failed to load status</div>;

  return (
    <div className="page">
      <h1>System Status</h1>

      <div className="status-grid">
        <div className="status-section">
          <h2>General</h2>
          <dl className="kv">
            <dt>Version</dt><dd>{status.version}</dd>
            <dt>Uptime</dt><dd>{fmtUptime(status.uptime_seconds)}</dd>
            <dt>Sessions (total)</dt><dd>{status.sessions_total}</dd>
            <dt>Sessions (today)</dt><dd>{status.sessions_today}</dd>
          </dl>
        </div>

        <div className="status-section">
          <h2>Platforms</h2>
          <table className="data-table compact">
            <thead>
              <tr><th>Platform</th><th>Enabled</th><th>Connected</th><th>Last Event</th></tr>
            </thead>
            <tbody>
              {status.platforms.map(p => (
                <tr key={p.name}>
                  <td>{p.name}</td>
                  <td>{p.enabled ? '✅' : '❌'}</td>
                  <td>{p.connected ? '🟢' : '🔴'}</td>
                  <td>{p.last_event || '—'}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>

        <div className="status-section">
          <h2>Cron Jobs</h2>
          <table className="data-table compact">
            <thead>
              <tr><th>Name</th><th>Schedule</th><th>Enabled</th><th>Last Run</th><th>Next Run</th></tr>
            </thead>
            <tbody>
              {cronJobs.map(j => (
                <tr key={j.id}>
                  <td>{j.name}</td>
                  <td><code>{j.schedule}</code></td>
                  <td>{j.enabled ? '✅' : '❌'}</td>
                  <td>{j.last_run || '—'}</td>
                  <td>{j.next_run || '—'}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
