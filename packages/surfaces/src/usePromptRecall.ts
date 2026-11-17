import { useEffect, useRef, type KeyboardEvent } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import type { VimMode } from './composer-vim';
import { caretOnFirstLine, caretOnLastLine, readPromptHistory, recallOrder } from './prompt-history';
import { useRunner } from './RunnerContext';
import type { TranscriptMessage } from './transcript-model';

type Recall = { list: string[]; index: number; draft: string };

/**
 * ↑/↓ recall sent messages, like a shell: this conversation's first, then
 * every other's. Browsing starts in an empty box (or with the caret at its
 * very start); the draft it replaced comes back below the newest entry.
 */
export function usePromptRecall({ viewState, sessionID, conversationKey, conversation, onRecall }: {
  viewState?: ViewStateStore;
  sessionID: string;
  conversationKey: string;
  conversation: readonly TranscriptMessage[];
  onRecall(text: string): void;
}) {
  // The Runner's prompt log adds what other windows and devices sent.
  const { api } = useRunner();
  const recall = useRef<Recall | null>(null);
  useEffect(() => { recall.current = null; }, [sessionID]);

  /** Handles an arrow key that browses the history; false leaves the key to the textarea. */
  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>, vimMode?: VimMode) => {
    if (event.key !== 'ArrowUp' && event.key !== 'ArrowDown') return false;
    const native = event.nativeEvent, el = event.currentTarget;
    if (native.isComposing || event.altKey || event.metaKey || event.ctrlKey || event.shiftKey || (vimMode && vimMode !== 'insert')) return false;
    const { selectionStart: start, selectionEnd: end, value } = el;
    let state = recall.current;
    if (event.key === 'ArrowUp') {
      if (!state) {
        if (value && !(start === 0 && end === 0)) return false;
        const list = recallOrder(readPromptHistory(viewState, api), conversationKey, conversation.filter(m => m.type === 'operation_accepted' && !m.origin).map(m => m.text));
        if (!list.length) return false;
        state = { list, index: -1, draft: value };
      } else if (!caretOnFirstLine(value, start, end)) return false;
      if (state.index + 1 >= state.list.length) { event.preventDefault(); return true; }
      state = { ...state, index: state.index + 1 };
    } else {
      if (!state || !caretOnLastLine(value, start, end)) return false;
      state = { ...state, index: state.index - 1 };
    }
    event.preventDefault();
    const text = state.index < 0 ? state.draft : state.list[state.index];
    recall.current = state.index < 0 ? null : state;
    // The text and caret are placed now, not a frame later: a late placement
    // would move the caret, or collapse the selection, of whatever was typed
    // or selected in between. The render that follows finds the box current.
    el.value = text; el.setSelectionRange(text.length, text.length); el.scrollTop = el.scrollHeight;
    onRecall(text);
    return true;
  };

  /** Typing or sending ends browsing. */
  const reset = () => { recall.current = null; };

  return { onKeyDown, reset };
}
