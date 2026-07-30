export interface ToolStatus {
  id: string;
  label: string;
  available: boolean;
  detail: string;
}

export interface DesktopSetupStatus {
  completed: boolean;
  github: ToolStatus;
  agents: ToolStatus[];
  selected_scopes: string[];
  default_agent: string | null;
  analytics_enabled: boolean;
  crash_reports_enabled: boolean;
}

export interface DesktopScope {
  id: string;
  label: string;
  parent: string | null;
}

export interface DesktopSetupInput {
  github_scopes: string[];
  default_agent: string;
  analytics_enabled: boolean;
  crash_reports_enabled: boolean;
}

export type AnalyticsEvent =
  | "onboarding_completed"
  | "workspace_opened"
  | "agent_started"
  | "shell_started"
  | "reply_posted";

export function canCompleteSetup(
  status: DesktopSetupStatus,
  input: DesktopSetupInput,
): boolean {
  return (
    status.github.available &&
    status.agents.some(
      (agent) => agent.available && agent.id === input.default_agent,
    ) &&
    input.github_scopes.length > 0
  );
}

export function mergeRepositoryScopes(
  current: Iterable<DesktopScope>,
  incoming: Iterable<DesktopScope>,
): DesktopScope[] {
  const scopes = new Map<string, DesktopScope>();
  for (const scope of [...current, ...incoming]) {
    scopes.set(scope.id, scope);
  }
  return [...scopes.values()].sort((left, right) =>
    left.label.localeCompare(right.label),
  );
}
