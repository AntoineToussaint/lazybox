import {
  isPermissionGranted,
  onAction,
  requestPermission,
  sendNotification,
  type PermissionState,
} from "@tauri-apps/plugin-notification";
import { getCurrentWindow } from "@tauri-apps/api/window";

export type { PermissionState };

export type AttentionReason = "asking" | "review" | "ci";

export interface NotificationPreferences {
  enabled: boolean;
  previews: boolean;
  quietWhenFocused: boolean;
}

export interface AttentionSignal {
  workspaceKey: string;
  workspaceName: string;
  reason: AttentionReason;
  fingerprint: string;
}

export interface NotificationBackend {
  permission(request: boolean): Promise<PermissionState>;
  send(options: {
    id: number;
    title: string;
    body: string;
    workspaceKey: string;
  }): void;
  setBadgeCount(count: number): Promise<void>;
  isFocused(): Promise<boolean>;
  focus(): Promise<void>;
  onClick(callback: (workspaceKey: string) => void): Promise<void>;
}

export function nativeNotificationBackend(): NotificationBackend {
  const window = getCurrentWindow();
  return {
    async permission(request) {
      if (await isPermissionGranted()) {
        return "granted";
      }
      if (!request) {
        return "prompt";
      }
      const permission = await requestPermission();
      return permission === "default" ? "prompt" : permission;
    },
    send(options) {
      sendNotification({
        id: options.id,
        title: options.title,
        body: options.body,
        autoCancel: true,
        extra: { workspaceKey: options.workspaceKey },
      });
    },
    async setBadgeCount(count) {
      await window.setBadgeCount(count === 0 ? undefined : count);
    },
    isFocused: () => window.isFocused(),
    async focus() {
      await window.unminimize();
      await window.show();
      await window.setFocus();
    },
    async onClick(callback) {
      await onAction((notification) => {
        const workspaceKey = notification.extra?.workspaceKey;
        if (typeof workspaceKey === "string") {
          callback(workspaceKey);
        }
      });
    },
  };
}

export function browserNotificationBackend(): NotificationBackend {
  let click: ((workspaceKey: string) => void) | undefined;
  return {
    async permission(request) {
      const current = Notification.permission;
      if (current !== "default" || !request) {
        return current === "default" ? "prompt" : current;
      }
      const permission = await Notification.requestPermission();
      return permission === "default" ? "prompt" : permission;
    },
    send(options) {
      const notification = new Notification(options.title, {
        body: options.body,
      });
      notification.onclick = () => click?.(options.workspaceKey);
    },
    setBadgeCount: async () => {},
    isFocused: async () => document.hasFocus(),
    focus: async () => window.focus(),
    async onClick(callback) {
      click = callback;
    },
  };
}

const titles: Record<AttentionReason, string> = {
  asking: "Agent needs input",
  review: "Review needs attention",
  ci: "CI needs attention",
};

export class AttentionNotifier {
  private readonly active = new Map<string, Set<AttentionReason>>();
  private readonly delivered = new Set<string>();
  private preferences: NotificationPreferences;

  constructor(
    private readonly backend: NotificationBackend,
    preferences: NotificationPreferences,
    private readonly route: (workspaceKey: string) => void,
  ) {
    this.preferences = preferences;
  }

  async initialize(): Promise<PermissionState> {
    await this.backend.onClick((workspaceKey) => {
      void this.backend.focus();
      this.route(workspaceKey);
    });
    return this.backend.permission(false);
  }

  setPreferences(preferences: NotificationPreferences): void {
    this.preferences = preferences;
  }

  permission(request = false): Promise<PermissionState> {
    return this.backend.permission(request);
  }

  async signal(signal: AttentionSignal): Promise<void> {
    const reasons = this.active.get(signal.workspaceKey) ?? new Set();
    reasons.add(signal.reason);
    this.active.set(signal.workspaceKey, reasons);
    await this.updateBadge();

    const deliveryKey = `${signal.workspaceKey}:${signal.reason}:${signal.fingerprint}`;
    if (this.delivered.has(deliveryKey)) {
      return;
    }
    this.delivered.add(deliveryKey);
    if (!this.preferences.enabled) {
      return;
    }
    if (this.preferences.quietWhenFocused && (await this.backend.isFocused())) {
      return;
    }
    if ((await this.backend.permission(false)) !== "granted") {
      return;
    }
    this.backend.send({
      id: notificationId(deliveryKey),
      title: titles[signal.reason],
      body: this.preferences.previews
        ? signal.workspaceName
        : "Open lazybox to view the workspace.",
      workspaceKey: signal.workspaceKey,
    });
  }

  async clear(workspaceKey: string, reason?: AttentionReason): Promise<void> {
    if (reason === undefined) {
      this.active.delete(workspaceKey);
    } else {
      const reasons = this.active.get(workspaceKey);
      reasons?.delete(reason);
      if (reasons?.size === 0) {
        this.active.delete(workspaceKey);
      }
    }
    await this.updateBadge();
  }

  async replaceActive(
    workspaces: Map<string, AttentionReason[]>,
  ): Promise<void> {
    this.active.clear();
    for (const [key, reasons] of workspaces) {
      if (reasons.length > 0) {
        this.active.set(key, new Set(reasons));
      }
    }
    await this.updateBadge();
  }

  private updateBadge(): Promise<void> {
    return this.backend.setBadgeCount(this.active.size);
  }
}

function notificationId(value: string): number {
  let hash = 2_166_136_261;
  for (const byte of new TextEncoder().encode(value)) {
    hash ^= byte;
    hash = Math.imul(hash, 16_777_619);
  }
  return hash >>> 0;
}
