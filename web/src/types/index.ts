export interface Session {
  id: string;
  title: string;
  created_at: string;
  updated_at: string;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  model: string;
  platform: string;
}

export interface SystemStatus {
  version: string;
  uptime_seconds: number;
  sessions_total: number;
  sessions_today: number;
  tokens_total: number;
  cron_jobs: number;
  cron_active: number;
  plugins: number;
  plugins_active: number;
  disk_usage_bytes: number;
  platforms: PlatformStatus[];
}

export interface PlatformStatus {
  name: string;
  enabled: boolean;
  connected: boolean;
  last_event?: string;
}

export interface CronJob {
  id: string;
  name: string;
  schedule: string;
  enabled: boolean;
  last_run?: string;
  next_run?: string;
}
