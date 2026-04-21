import { useEffect, useState, useMemo } from 'react';
import { Link } from 'react-router-dom';
import type { Session } from '../types';
import { api, mockApi, safeApi } from '../api/client';

function fmtTokens(n: number): string {
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}K`;
  return `${n}`;
}

export default function Sessions() {
  const [sessions, setSessions] = useState<Session[]>([]);
  const [query, setQuery] = useState('');
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    safeApi(api.getSessions, mockApi.getSessions)
      .then(setSessions)
      .finally(() => setLoading(false));
  }, []);

  const filtered = useMemo(() => {
    const q = query.toLowerCase();
    return sessions.filter(s =>
      s.title.toLowerCase().includes(q) ||
      s.platform.toLowerCase().includes(q) ||
      s.model.toLowerCase().includes(q)
    );
  }, [sessions, query]);

  if (loading) return <div className="loading">Loading…</div>;

  return (
    <div className="page">
      <h1>Sessions</h1>
      <input
        className="search"
        type="text"
        placeholder="Search by title, platform or model…"
        value={query}
        onChange={e => setQuery(e.target.value)}
      />
      <table className="data-table">
        <thead>
          <tr>
            <th>Title</th>
            <th>Platform</th>
            <th>Model</th>
            <th>Input</th>
            <th>Output</th>
            <th>Cache</th>
            <th>Created</th>
          </tr>
        </thead>
        <tbody>
          {filtered.map(s => (
            <tr key={s.id}>
              <td><Link to={`/sessions/${s.id}`}>{s.title}</Link></td>
              <td><span className={`badge platform-${s.platform}`}>{s.platform}</span></td>
              <td>{s.model}</td>
              <td>{fmtTokens(s.input_tokens)}</td>
              <td>{fmtTokens(s.output_tokens)}</td>
              <td>{fmtTokens(s.cache_read_tokens + s.cache_write_tokens)}</td>
              <td>{new Date(s.created_at).toLocaleString()}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="meta">{filtered.length} session(s)</p>
    </div>
  );
}
