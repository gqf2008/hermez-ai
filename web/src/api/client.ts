import type { Session, SystemStatus, CronJob } from '../types';

const API_BASE = import.meta.env.VITE_API_BASE || '/api';

async function fetchJson<T>(path: string): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`);
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.json() as Promise<T>;
}

export const api = {
  getStatus: () => fetchJson<SystemStatus>('/status'),
  getSessions: () => fetchJson<Session[]>('/sessions'),
  getCronJobs: () => fetchJson<CronJob[]>('/cron'),
};

// Mock data for development when backend is not running
export const mockApi = {
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
