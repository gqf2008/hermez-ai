import type { Session, SystemStatus, CronJob } from '../types';

const API_BASE = import.meta.env.VITE_API_BASE || '/api';

async function fetchJson<T>(path: string): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`);
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.json() as Promise<T>;
}

export interface SessionDetail {
  session: Session | null;
  messages: Array<{
    role: string;
    content?: string;
    tool_call_id?: string;
    tool_calls?: string;
    tool_name?: string;
    reasoning?: string;
    reasoning_details?: string;
    codex_reasoning_items?: string;
  }>;
}

export const api = {
  getStatus: () => fetchJson<SystemStatus>('/status'),
  getSessions: () => fetchJson<Session[]>('/sessions'),
  getSession: (id: string) => fetchJson<SessionDetail>(`/sessions/${id}`),
  deleteSession: (id: string) => fetch(`${API_BASE}/sessions/${id}`, { method: 'DELETE' }).then(r => r.json()),
  renameSession: (id: string, title: string) =>
    fetch(`${API_BASE}/sessions/${id}/rename`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ title }),
    }).then(r => r.json()),
  getConfig: () => fetchJson<Record<string, unknown>>('/config'),
  saveConfig: (config: Record<string, unknown>) =>
    fetch(`${API_BASE}/config`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(config),
    }).then(r => r.json()),
  getPlugins: () => fetchJson<Array<{
    name: string;
    version: string;
    description: string;
    author: string;
    tools: string[];
    hooks: string[];
    wasm_entry?: string;
    component_entry?: string;
  }>>('/plugins'),
  getCronJobs: () => fetchJson<CronJob[]>('/cron'),
  exportSession: (id: string) =>
    fetch(`${API_BASE}/sessions/${id}/export`).then(r => {
      if (!r.ok) throw new Error(`HTTP ${r.status}`);
      return r.blob();
    }),
  chat: (id: string, message: string, systemPrompt?: string) =>
    fetch(`${API_BASE}/sessions/${id}/chat`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ message, system_prompt: systemPrompt }),
    }).then(r => r.json()),
  chatStream: async (
    id: string,
    message: string,
    systemPrompt?: string,
    onDelta?: (delta: string) => void,
    onDone?: (result: { response: string; api_calls: number; exit_reason: string }) => void,
    onError?: (error: string) => void,
  ) => {
    const res = await fetch(`${API_BASE}/sessions/${id}/chat-stream`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ message, system_prompt: systemPrompt }),
    });
    if (!res.ok || !res.body) {
      onError?.(`HTTP ${res.status}`);
      return;
    }
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    let currentEvent = 'delta';
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split('\n');
      buffer = lines.pop() || '';
      for (let i = 0; i < lines.length; i++) {
        const line = lines[i];
        if (line.startsWith('event: ')) {
          currentEvent = line.slice(7).trim();
          continue;
        }
        if (line.startsWith('data: ')) {
          const data = line.slice(6);
          if (currentEvent === 'error') {
            onError?.(data);
          } else if (currentEvent === 'done') {
            try {
              const parsed = JSON.parse(data);
              onDone?.(parsed);
            } catch {
              onDone?.({ response: data, api_calls: 0, exit_reason: 'unknown' });
            }
          } else {
            onDelta?.(data);
          }
          continue;
        }
        if (line.trim() === '') {
          currentEvent = 'delta';
        }
      }
    }
  },
  createSession: (title: string, model?: string) =>
    fetch(`${API_BASE}/sessions`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ title, model }),
    }).then(r => r.json()),
};

// Mock data for development when backend is not running
export const mockApi = {
  getSession: async (_id: string): Promise<SessionDetail> => ({
    session: {
      id: 'sess-1', title: 'Code review assistant', created_at: '2025-04-19T10:00:00Z',
      updated_at: '2025-04-19T10:30:00Z', input_tokens: 1200, output_tokens: 800,
      cache_read_tokens: 0, cache_write_tokens: 0, model: 'claude-sonnet-4', platform: 'feishu'
    },
    messages: [
      { role: 'user', content: 'Review this PR for security issues' },
      { role: 'assistant', content: 'I\'ve analyzed the PR. Here are my findings...\n\n1. No SQL injection risks found.\n2. Input validation looks solid.\n3. One minor issue: missing rate limiting on the login endpoint.' },
      { role: 'user', content: 'Can you suggest a fix for the rate limiting?' },
      { role: 'assistant', content: 'Sure! You can use a token bucket algorithm...' },
    ],
  }),
  getStatus: async (): Promise<SystemStatus> => ({
    version: '0.1.0',
    uptime_seconds: 3600,
    sessions_total: 42,
    sessions_today: 5,
    tokens_total: 1_250_000,
    cron_jobs: 3,
    cron_active: 2,
    plugins: 5,
    plugins_active: 4,
    disk_usage_bytes: 45_000_000,
    platforms: [
      { name: 'Feishu', enabled: true, connected: true, last_event: '2 min ago' },
      { name: 'WeChat', enabled: true, connected: true, last_event: '5 min ago' },
      { name: 'WeCom', enabled: true, connected: false },
      { name: 'QQ Bot', enabled: false, connected: false },
      { name: 'Discord', enabled: false, connected: false },
      { name: 'Slack', enabled: false, connected: false },
      { name: 'Telegram', enabled: false, connected: false },
    ],
  }),
  getSessions: async (): Promise<Session[]> => [
    {
      id: 'sess-1', title: 'Code review assistant', created_at: '2025-04-19T10:00:00Z',
      updated_at: '2025-04-19T10:30:00Z', input_tokens: 1200, output_tokens: 800,
      cache_read_tokens: 0, cache_write_tokens: 0, model: 'claude-sonnet-4', platform: 'feishu'
    },
    {
      id: 'sess-2', title: 'Documentation generation', created_at: '2025-04-19T09:00:00Z',
      updated_at: '2025-04-19T09:45:00Z', input_tokens: 3400, output_tokens: 2100,
      cache_read_tokens: 500, cache_write_tokens: 200, model: 'gpt-4o', platform: 'wechat'
    },
    {
      id: 'sess-3', title: 'Bug analysis', created_at: '2025-04-18T16:00:00Z',
      updated_at: '2025-04-18T16:20:00Z', input_tokens: 800, output_tokens: 400,
      cache_read_tokens: 0, cache_write_tokens: 0, model: 'claude-haiku-3', platform: 'api'
    },
  ],
  createSession: async (_title: string, _model?: string): Promise<{ id: string; title: string }> => ({
    id: 'sess-new-' + Math.random().toString(36).slice(2, 8),
    title: _title,
  }),
  getPlugins: async (): Promise<Array<{
    name: string;
    version: string;
    description: string;
    author: string;
    tools: string[];
    hooks: string[];
    wasm_entry?: string;
    component_entry?: string;
  }>> => ([
    {
      name: 'calc-plugin',
      version: '0.1.0',
      description: 'Mathematical expression calculator for Hermes',
      author: 'Hermes Team',
      tools: ['calc'],
      hooks: ['on_session_start', 'on_session_end'],
      component_entry: 'plugin.component.wasm',
    },
    {
      name: 'example-wasm-plugin',
      version: '0.1.0',
      description: 'Example WASM plugin for Hermes',
      author: 'Hermes Team',
      tools: ['greet'],
      hooks: ['on_session_start'],
      wasm_entry: 'plugin.wasm',
    },
  ]),
  getConfig: async (): Promise<Record<string, unknown>> => ({
    agent: { model: 'anthropic/claude-opus-4-6', provider: 'anthropic', toolsets: ['filesystem', 'web', 'terminal'] },
    terminal: { backend: 'local' },
    compression: { enabled: true, target_tokens: 50 },
  }),
  getCronJobs: async (): Promise<CronJob[]> => [
    { id: 'cron-1', name: 'Daily summary', schedule: '0 9 * * *', enabled: true, last_run: '2025-04-19 09:00', next_run: '2025-04-20 09:00' },
    { id: 'cron-2', name: 'Health check', schedule: '*/5 * * * *', enabled: true, last_run: '2025-04-19 14:55', next_run: '2025-04-19 15:00' },
    { id: 'cron-3', name: 'Backup', schedule: '0 2 * * 0', enabled: false },
  ],
};

// Use mock API if real API is unavailable
export async function safeApi<T>(
  real: () => Promise<T>,
  mock: () => Promise<T>
): Promise<T> {
  try {
    return await real();
  } catch {
    return await mock();
  }
}
