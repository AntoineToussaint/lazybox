import { describe, expect, it, vi } from "vitest";
import {
  AttentionNotifier,
  type NotificationBackend,
  type NotificationPreferences,
} from "./notifications";

function harness(overrides: Partial<NotificationBackend> = {}) {
  let click: ((workspaceKey: string) => void) | undefined;
  const backend: NotificationBackend = {
    permission: vi.fn().mockResolvedValue("granted"),
    send: vi.fn(),
    setBadgeCount: vi.fn().mockResolvedValue(undefined),
    isFocused: vi.fn().mockResolvedValue(false),
    focus: vi.fn().mockResolvedValue(undefined),
    onClick: vi.fn(async (callback) => {
      click = callback;
    }),
    ...overrides,
  };
  const route = vi.fn();
  const preferences: NotificationPreferences = {
    enabled: true,
    previews: false,
    quietWhenFocused: true,
  };
  return {
    backend,
    route,
    notifier: new AttentionNotifier(backend, preferences, route),
    click: (key: string) => click?.(key),
  };
}

describe("AttentionNotifier", () => {
  it("deduplicates reconnect replay and counts each workspace once", async () => {
    const { notifier, backend } = harness();
    const signal = {
      workspaceKey: "github:o/r#1",
      workspaceName: "Private title",
      reason: "asking" as const,
      fingerprint: "terminal-7-input",
    };
    await notifier.signal(signal);
    await notifier.signal(signal);
    await notifier.signal({ ...signal, reason: "ci", fingerprint: "sha-1" });

    expect(backend.send).toHaveBeenCalledTimes(2);
    expect(backend.setBadgeCount).toHaveBeenLastCalledWith(1);
  });

  it("does not leak task content when previews are disabled", async () => {
    const { notifier, backend } = harness();
    await notifier.signal({
      workspaceKey: "secret",
      workspaceName: "Do not show this title",
      reason: "review",
      fingerprint: "review-1",
    });

    expect(backend.send).toHaveBeenCalledWith(
      expect.objectContaining({ body: "Open lazybox to view the workspace." }),
    );
    expect(JSON.stringify(vi.mocked(backend.send).mock.calls)).not.toContain(
      "Do not show this title",
    );
  });

  it("stays quiet while focused and when permission is unavailable", async () => {
    const focused = harness({ isFocused: vi.fn().mockResolvedValue(true) });
    await focused.notifier.signal({
      workspaceKey: "one",
      workspaceName: "one",
      reason: "asking",
      fingerprint: "1",
    });
    expect(focused.backend.send).not.toHaveBeenCalled();

    const denied = harness({ permission: vi.fn().mockResolvedValue("denied") });
    await denied.notifier.signal({
      workspaceKey: "two",
      workspaceName: "two",
      reason: "ci",
      fingerprint: "2",
    });
    expect(denied.backend.send).not.toHaveBeenCalled();
  });

  it("focuses and routes an activated notification to its workspace", async () => {
    const { notifier, backend, route, click } = harness();
    await notifier.initialize();
    click("github:o/r#9");
    await Promise.resolve();

    expect(backend.focus).toHaveBeenCalled();
    expect(route).toHaveBeenCalledWith("github:o/r#9");
  });
});
