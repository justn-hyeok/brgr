export type AgentOutcome = "completed" | "failed" | "cancelled" | "lost";

export interface AgentOptions {
  task: string;
  harness?: string;
  model?: string;
  effort?: string;
  mode?: string;
  key?: string;
  criteria?: string[];
  deadline?: number;
  retries?: number;
}

export interface AgentResult {
  outcome: AgentOutcome;
  report: string;
  task_id: string;
  revision: number;
  result_id: string;
  timed_out: boolean;
  attempts: number;
}

export interface PipelineOptions {
  key?: string;
}

export interface PipelineResult {
  results: AgentResult[];
  stopped_at: number | null;
}

export type PipelineStage = (
  previous: AgentResult | undefined,
) => Promise<AgentResult> | AgentResult;

export function agent(options: AgentOptions): Promise<AgentResult>;
export function parallel<T>(thunks: Array<() => Promise<T> | T>): Promise<T[]>;
export function pipeline(
  stages: PipelineStage[],
  options?: PipelineOptions,
): Promise<PipelineResult>;
