import { useEffect, useState } from 'react';
import { api, mockApi, safeApi } from '../api/client';

interface Plugin {
  name: string;
  version: string;
  description: string;
  author: string;
  tools: string[];
  hooks: string[];
  wasm_entry?: string;
  component_entry?: string;
}

export default function Plugins() {
  const [plugins, setPlugins] = useState<Plugin[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    safeApi(api.getPlugins, mockApi.getPlugins)
      .then(setPlugins)
      .finally(() => setLoading(false));
  }, []);

  if (loading) return <div className="loading">Loading…</div>;

  return (
    <div className="page">
      <h1>Plugins</h1>
      <p className="meta">{plugins.length} plugin(s) discovered in ~/.hermes/plugins/</p>

      {plugins.length === 0 ? (
        <div className="empty-state">
          <p>No plugins found.</p>
          <p className="meta">
            Place plugin directories under <code>~/.hermes/plugins/</code>.
            Each directory should contain a <code>plugin.yaml</code> manifest and a <code>.wasm</code> file.
          </p>
        </div>
      ) : (
        <div className="plugin-grid">
          {plugins.map(p => (
            <div key={p.name} className="plugin-card">
              <div className="plugin-header">
                <h3>{p.name}</h3>
                <span className="badge">v{p.version}</span>
              </div>
              <p className="plugin-desc">{p.description}</p>
              <div className="plugin-meta">
                <span>by {p.author}</span>
              </div>
              {p.tools.length > 0 && (
                <div className="plugin-tags">
                  <span className="tag-label">Tools:</span>
                  {p.tools.map(t => (
                    <span key={t} className="badge">{t}</span>
                  ))}
                </div>
              )}
              {p.hooks.length > 0 && (
                <div className="plugin-tags">
                  <span className="tag-label">Hooks:</span>
                  {p.hooks.map(h => (
                    <span key={h} className="badge hook">{h}</span>
                  ))}
                </div>
              )}
              <div className="plugin-tech">
                {p.component_entry && <span className="tech-badge component">Component</span>}
                {p.wasm_entry && <span className="tech-badge wasm">WASM</span>}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
