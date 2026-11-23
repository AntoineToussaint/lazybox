/** An error in the words the Runner shows the person: its message, without JavaScript's "Error: " prefix. */
export const errorText = (error: unknown): string => error instanceof Error ? error.message : String(error);
