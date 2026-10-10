export interface Config {
  base_path: string;
  version: string;
  api_base: string;
  auth_mode: string;
}

export interface DefinitionSpec {
  name: string;
  description: string;
  driver: "node" | "python";
  script: string;
  input_schema: Record<string, unknown>;
  required_tags: string[];
  timeout_seconds: number;
}

export interface Definition {
  id: string;
  revision: number;
  spec: DefinitionSpec;
  created_at: string;
  updated_at: string;
}

export type RunStatus = "queued" | "assigned" | "running" | "succeeded" | "failed" | "canceled";

export interface Run {
  id: string;
  definition_id: string;
  definition_name: string;
  revision: number;
  status: RunStatus;
  worker_id: string | null;
  attempt_id: string | null;
  input: Record<string, unknown>;
  result: unknown;
  error: string | null;
  exit_code: number | null;
  created_at: string;
  started_at: string | null;
  finished_at: string | null;
  cancel_requested: boolean;
  waiting_reason: string | null;
  spec?: DefinitionSpec;
  files?: Array<{ name: string; artifact_id: string }>;
}

export interface Worker {
  id: string;
  name: string;
  version: string;
  tags: string[];
  drivers: string[];
  status: string;
  enabled: boolean;
  last_seen: string;
  active_run_id: string | null;
}

export interface Token {
  id: string;
  name: string;
  kind: "api" | "worker";
  created_at: string;
  revoked: boolean;
}

export interface Settings {
  scheduler_paused: boolean;
  version: string;
  base_path: string;
  heartbeat_timeout_seconds: number;
  max_artifact_bytes: number;
  storage: unknown;
  auto_retry: boolean;
}

export interface Artifact {
  id: string;
  name: string;
  size: number;
  url: string;
}
