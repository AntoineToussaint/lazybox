// Types of the Lazybox extension bridge, protocol 1 (EXTENSIONS.md).
// `lazybox extension new` writes this file beside a new extension; reference
// it from your editor (/// <reference path="lazybox.d.ts" />) for completion.

declare namespace Lazybox {
  /** Why a call was refused. */
  type ErrorCode = 'not-granted' | 'bad-request' | 'unknown-method' | 'unsupported-protocol' | 'rate-limited' | 'too-large' | 'refused' | 'failed' | 'timeout' | 'unavailable';
  interface LazyboxError extends Error { name: 'LazyboxError'; code: ErrorCode; /** The grant the call needed, for not-granted. */ grant?: string }

  /** The person's theme: set as CSS custom properties on <html> by lazybox.js. */
  interface Theme {
    scheme: 'dark' | 'light';
    /** Token name (no leading --) → CSS colour, such as surface, text-muted, accent. */
    tokens: Record<string, string>;
    fonts: { ui: string; code: string };
    /** The interface text size as a factor of 13 px. */
    textScale: number;
    density: 'compact' | 'comfortable' | 'relaxed';
  }

  interface Hello {
    protocol: 1;
    extension: { id: string; name: string; version: string };
    panel: { id: string; title: string };
    /** What this panel may do now: the grants the person approved. */
    grants: string[];
    hosts: string[];
    runnerLabel?: string;
    theme: Theme;
  }

  interface Workspace { id: string; name: string; repositories: string[] }
  type ConversationStatus = 'starting' | 'running' | 'waiting_input' | 'waiting_approval' | 'idle' | 'done' | 'failed' | 'stopped' | 'unknown' | string;
  interface Conversation {
    sessionId: string; conversationId: string; title: string;
    workspaceId: string; workspaceName: string;
    provider: string; model: string; status: ConversationStatus;
    busy: boolean; closed: boolean;
    /** Unix milliseconds. */
    createdAt: number; updatedAt: number;
  }
  type AttentionState = 'needs-input' | 'failed' | 'review' | 'working' | 'idle';
  interface AttentionItem {
    sessionId: string; conversationId: string; title: string;
    workspaceId: string; workspaceName: string;
    state: AttentionState;
    /** One short phrase: "Approval", "Question · 2 waiting", "Turn ended". */
    reason: string;
    /** The pending question, the failure, or the agent's last line. */
    line: string;
    /** Newest activity, Unix milliseconds. */
    at: number;
    /** Not yet seen by the person. */
    unread: boolean;
  }
  interface Command { id: string; title: string; state: 'running' | 'succeeded' | 'failed' | 'stopped'; exitCode?: number; startedAt: number }
  interface Mission { id: string; title: string; canceled: boolean; awaitingReview: number; tasks: { id: string; title: string; status: string; reason: string }[] }
  interface UsageGroup {
    workspaceId: string; workspaceName: string; conversationId: string; model: string; provider: string;
    inputTokens: number; outputTokens: number; cacheReadTokens: number; cacheWriteTokens: number;
    /** Dollars; null when unpriced. */
    usd: number | null; records: number;
  }
  /** Usage in one hour: `at` is the hour's start (UTC), Unix milliseconds. */
  interface UsageHour extends UsageGroup { at: number; unpriced: number }
  /** `inputTokens` is uncached input; the cache's reads and writes are counted apart. */
  interface Usage {
    totalUsd: number; unpriced: number; groups: UsageGroup[];
    /** Hour by hour, oldest first; only when read with `series: true`. */
    series?: UsageHour[];
  }

  interface Api {
    readonly protocol: 1;
    /** Resolves once Lazybox answered hello: the extension, its grants and the theme. */
    readonly ready: Promise<Hello>;
    readonly theme: Theme | null;
    call<T = unknown>(method: string, params?: object): Promise<T>;
    /** Runs fn now and every ms (at least 1 s) while the panel is visible. Returns stop. */
    every(ms: number, fn: () => unknown): () => void;
    on(event: 'theme', listener: (theme: Theme) => void): () => void;
    on(event: string, listener: (data: unknown) => void): () => void;
    /** read:workspaces */
    workspaces: { list(): Promise<{ workspaces: Workspace[] }>; focused(): Promise<{ workspace: Workspace | null }> };
    runtime: {
      /** read:runtime — status and ports, without script bodies or logs. */
      read(workspaceId: string): Promise<{
        services: { id: string; name: string; member: string; port: number; preview: boolean; state: string; hasLog: boolean }[];
        setups: { member: string; pendingApproval: boolean; state: string; copied: number; missing: number }[];
      }>;
      /** show:runtime + read:runtime — opens an approved workspace UI, where a person may act. */
      show(workspaceId: string, kind: 'preview' | 'setup' | 'logs', target: string): Promise<{ shown: true }>;
    };
    /** show:github — opens a GitHub issue, pull request or Actions run. */
    github: { open(url: string): Promise<{ shown: true }> };
    /** read:releases — deployments for workspace repositories. */
    releases: { list(): Promise<{ items: { id: string; repository: string; environment: string; reason: string; url: string }[]; truncated: boolean }> };
    /** read:pulls — metadata for tracked pull requests. */
    pulls: { list(workspaceId?: string): Promise<{ pulls: { key: string; title: string; repository: string; number: number; status: string; detail: string; workspaceId: string; sessionId: string; url: string; checkUrl: string; checks: string }[]; error: string }> };
    artifacts: {
      /** read:artifacts — titles and IDs, without contents. */
      list(workspaceId: string): Promise<{ artifacts: { id: string; title: string; kind: string }[] }>;
      /** draft:artifact — opens a review dialog in the selected workspace. */
      draft(workspaceId: string, content: string): Promise<{ shown: true }>;
    };
    /** read:mailbox — unread notifications for the Runner's GitHub account. */
    mailbox: { list(): Promise<{ items: { id: string; repository: string; title: string; reason: string; url: string; updatedAt: string }[]; truncated: boolean }> };
    /** read:sessions — titles and status, never messages. */
    sessions: { list(options?: { workspaceId?: string; limit?: number }): Promise<{ conversations: Conversation[] }> };
    /** read:attention — what needs the person, as the Inbox shows it. */
    attention: { list(): Promise<{ items: AttentionItem[]; counts: Record<AttentionState, number> }> };
    /** read:commands — a conversation's command runs, never their output. */
    commands: { list(sessionId: string): Promise<{ commands: Command[] }> };
    /** read:missions */
    missions: { list(): Promise<{ missions: Mission[] }> };
    /** read:usage — tokens and cost by workspace, conversation and model; with series, hour by hour too. */
    usage: { read(options?: { workspaceId?: string; series?: boolean }): Promise<Usage> };
    /** show:conversations — shows a conversation in the window. */
    conversations: { show(sessionId: string): Promise<{ shown: true }> };
    /** send:messages — sends a message to a conversation, as the person. */
    messages: { send(sessionId: string, text: string): Promise<{ sent: true }> };
    /** do:<action> — runs one action of the app marked safe (lazybox do --list). */
    run(action: string, argument?: string): Promise<{ done: true; detail: string }>;
  }
}

declare const lazybox: Lazybox.Api;
