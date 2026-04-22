// API Types

export interface Host {
  host: string;
  hostname: string;
  connected_at: string;
}

export interface Environment {
  host: string;
  name: string;
  directory: string;
}

export interface BranchesResponse {
  branches: string[];
}

export interface CreateEnvironmentRequest {
  host: string;
  name: string;
}

export interface PromptRun {
  prompt: string;
  started_at: string;
  finished_at: string | null;
  success: boolean | null;
}

export interface Task {
  id: string;
  host: string;
  agent: AgentKind;
  environment: string;
  name: string;
  workspace_kind: "environment" | "worktree";
  base_branch?: string | null;
  merge_branch?: string | null;
  status: "pending" | "running" | "completed" | "failed" | "interrupted";
  session_id: string | null;
  created_at: string;
  worktree_date?: string | null;
  history: PromptRun[];
  // SlopCoderNG fields
  owner: string;
  workspace_slug: string;
  pod_name?: string | null;
  ssh_port?: number | null;
  ssh_command?: string | null;
  workspace_url?: string | null;
  app_url?: string | null;
  http_port: number;
  collaborators: string[];
}

export interface CreateTaskRequest {
  host: string;
  environment: string;
  name?: string;
  use_worktree?: boolean;
  web_search?: boolean;
  prompt: string;
  agent: AgentKind;
}

export interface CreateTaskResponse {
  id: string;
  worktree_path: string;
}

export interface RenameTaskRequest {
  name: string;
}

export interface SendPromptRequest {
  prompt: string;
}

export interface TaskOutputResponse {
  events: AgentEvent[];
  total_events: number;
  has_more_before: boolean;
}

export interface TaskDiffResponse {
  staged: string;
  unstaged: string;
}

// Codex Event Types (from WebSocket)

export interface CompletedItem {
  id: string;
  type: string;
  text?: string;
  name?: string;
  arguments?: string;
  call_id?: string;
  output?: string;
  command?: string;
  aggregated_output?: string;
  exit_code?: number;
  status?: string;
  stdout?: string;
  stderr?: string;
  changes?: Array<{
    kind: string;
    path: string;
  }>;
}

export interface UsageStats {
  input_tokens?: number;
  cached_input_tokens?: number;
  output_tokens?: number;
}

export type AgentKind = "codex" | "claude" | "cursor" | "opencode" | "gemini";

export function agentSupportsWebSearch(agent: AgentKind): boolean {
  return agent === "codex";
}

export type AgentEvent =
  | { type: "session.started"; session_id: string }
  | { type: "turn.started" }
  | { type: "item.completed"; item: CompletedItem }
  | { type: "turn.completed"; usage?: UsageStats }
  | { type: "background_event"; event?: string }
  | { type: "prompt.sent"; prompt: string }
  | { type: "unknown" };
