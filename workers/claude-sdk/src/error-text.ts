/**
 * An error in the words the Runner shows the person: its message, without
 * JavaScript's "Error: " prefix; its name when it has no message (an
 * AbortError), so it never reads as nothing.
 */
export const errorText = (error: unknown): string => {
  if (error instanceof Error) return error.message || error.name || String(error);
  return String(error);
};

/** A Runner call's refusal as the person reads it: the message inside the Runner's JSON answer, when it is one. */
export const errorReason = (error: unknown): string => {
  const message = errorText(error);
  try {
    const parsed: unknown = JSON.parse(message);
    const inner = parsed && typeof parsed === 'object' ? (parsed as { message?: unknown }).message : undefined;
    return typeof inner === 'string' && inner ? inner : message;
  } catch { return message; }
};
