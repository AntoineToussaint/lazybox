import { useSyncExternalStore } from 'react';

/**
 * In-app toasts: small, quiet cards in the corner of the window. The window
 * the person is looking at shows these instead of a system notification.
 */
export type ToastTone = 'needs-input' | 'failed' | 'review' | 'info';
export type Toast = {
  id: string;
  tone: ToastTone;
  title: string;
  body?: string;
  /** Clicking the toast (or its button) does this. */
  action?: { label: string; run(): void };
  /** A labelled button of its own beside the text (Undo), which also dismisses the toast. */
  button?: { label: string; run(): void };
  /** Shown as a hint, such as the key that jumps to the next waiting agent. */
  hint?: string;
  /** Milliseconds before it leaves on its own; paused while hovered or focused. */
  timeout: number;
};

let toasts: Toast[] = [];
const listeners = new Set<() => void>();
const emit = () => { for (const listener of listeners) listener(); };
const limit = 4;

export function showToast(toast: Omit<Toast, 'id' | 'timeout'> & { id?: string; timeout?: number }): string {
  const id = toast.id || crypto.randomUUID();
  // The same conversation replaces its previous toast rather than stacking.
  toasts = [...toasts.filter(value => value.id !== id), { timeout: toast.tone === 'info' ? 2600 : 8000, ...toast, id }].slice(-limit);
  emit();
  return id;
}
export function dismissToast(id: string) {
  if (!toasts.some(toast => toast.id === id)) return;
  toasts = toasts.filter(toast => toast.id !== id);
  emit();
}
export function useToasts(): Toast[] {
  return useSyncExternalStore(listener => { listeners.add(listener); return () => { listeners.delete(listener); }; }, () => toasts);
}
