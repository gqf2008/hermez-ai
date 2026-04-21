import { useEffect, useState } from 'react';
import { api, mockApi, safeApi } from '../api/client';

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

function ConfigTree({ data, path, onChange }: { data: unknown; path: string; onChange: (p: string, v: unknown) => void }) {
  if (data === null || data === undefined) {
    return <span className="config-null">{String(data)}</span>;
  }
  if (typeof data === 'boolean') {
    return (
      <label className="config-editable">
        <input
          type="checkbox"
          checked={data}
          onChange={e => onChange(path, e.target.checked)}
        />
        <span className="config-bool">{data ? 'true' : 'false'}</span>
      </label>
    );
  }
  if (typeof data === 'number') {
    return (
      <input
        className="config-input config-number"
        type="number"
        value={data}
        onChange={e => onChange(path, Number(e.target.value))}
      />
    );
  }
  if (typeof data === 'string') {
    return (
      <input
        className="config-input config-string"
        type="text"
        value={data}
        onChange={e => onChange(path, e.target.value)}
      />
    );
  }
  if (Array.isArray(data)) {
    return (
      <div style={{ paddingLeft: 12 }}>
        <span className="config-punct">[</span>
        {data.map((item, i) => (
          <div key={i} className="config-line">
            <ConfigTree data={item} path={`${path}[${i}]`} onChange={onChange} />
            {i < data.length - 1 && <span className="config-punct">,</span>}
          </div>
        ))}
        <span className="config-punct">]</span>
      </div>
    );
  }
  if (isObject(data)) {
    const entries = Object.entries(data);
    return (
      <div style={{ paddingLeft: 12 }}>
        <span className="config-punct">{'{'}</span>
        {entries.map(([key, value], i) => (
          <div key={key} className="config-line">
            <span className="config-key">{key}</span>
            <span className="config-punct">: </span>
            <ConfigTree data={value} path={path ? `${path}.${key}` : key} onChange={onChange} />
            {i < entries.length - 1 && <span className="config-punct">,</span>}
          </div>
        ))}
        <span className="config-punct">{'}'}</span>
      </div>
    );
  }
  return <span>{String(data)}</span>;
}

function setPath(obj: Record<string, unknown>, path: string, value: unknown): Record<string, unknown> {
  const result = JSON.parse(JSON.stringify(obj)) as Record<string, unknown>;
  const keys = path.split('.');
  let current: unknown = result;
  for (let i = 0; i < keys.length - 1; i++) {
    const key = keys[i];
    const next = (current as Record<string, unknown>)[key];
    if (isObject(next)) {
      current = next;
    } else {
      return result;
    }
  }
  const lastKey = keys[keys.length - 1];
  if (isObject(current)) {
    current[lastKey] = value;
  }
  return result;
}

export default function Settings() {
  const [config, setConfig] = useState<Record<string, unknown> | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [isSaving, setIsSaving] = useState(false);
  const [saveMsg, setSaveMsg] = useState<string | null>(null);

  useEffect(() => {
    safeApi(api.getConfig, mockApi.getConfig)
      .then(setConfig)
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
  }, []);

  const handleChange = (path: string, value: unknown) => {
    if (!config) return;
    setConfig(setPath(config, path, value));
  };

  const handleSave = async () => {
    if (!config) return;
    setIsSaving(true);
    setSaveMsg(null);
    try {
      const res = await api.saveConfig(config);
      if (res.saved) {
        setSaveMsg('Saved successfully');
      } else {
        setSaveMsg(`Save failed: ${res.error || 'unknown error'}`);
      }
    } catch (e) {
      setSaveMsg(`Save failed: ${String(e)}`);
    } finally {
      setIsSaving(false);
    }
  };

  if (loading) return <div className="loading">Loading…</div>;
  if (error) return <div className="error">{error}</div>;
  if (!config) return <div className="error">No config loaded</div>;

  return (
    <div className="page">
      <div className="dashboard-header">
        <h1>Settings</h1>
        <button className="btn primary" onClick={handleSave} disabled={isSaving}>
          {isSaving ? 'Saving…' : 'Save'}
        </button>
      </div>
      <p className="meta">Edit values inline and click Save to write ~/.hermes/config.yaml</p>
      {saveMsg && (
        <div className={`save-toast ${saveMsg.includes('failed') ? 'error' : 'success'}`}>
          {saveMsg}
        </div>
      )}

      <div className="config-viewer">
        <ConfigTree data={config} path="" onChange={handleChange} />
      </div>
    </div>
  );
}
