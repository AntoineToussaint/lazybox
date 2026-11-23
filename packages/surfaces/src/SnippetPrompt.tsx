import { createContext, useContext, useMemo } from 'react';
import type { Snippet } from '@lazybox/protocol';
import { CardBox } from './AttachmentBox';
import './snippet-prompt.css';

export const PromptSnippets = createContext<readonly Snippet[]>([]);

/** Recognize verbatim catalog entries, keeping surrounding words in their original order. */
export function useSnippetParts(text: string) {
  const entries = useContext(PromptSnippets);
  return useMemo(() => {
    const parts: (string | Snippet)[] = [];
    let start = 0;
    while (start < text.length) {
      let best: Snippet | undefined;
      let at = text.length;
      for (const entry of entries) {
        if (!entry.body.trim()) continue;
        let index = text.indexOf(entry.body, start);
        while (index >= 0) {
          const end = index + entry.body.length;
          if ((index === 0 || /\s/u.test(text[index - 1])) && (end === text.length || /\s/u.test(text[end]))) break;
          index = text.indexOf(entry.body, index + 1);
        }
        if (index >= 0 && (index < at || (index === at && entry.body.length > (best?.body.length ?? 0)))) {
          best = entry;
          at = index;
        }
      }
      if (!best) break;
      if (at > start) parts.push(text.slice(start, at));
      parts.push(best);
      start = at + best.body.length;
    }
    if (!parts.length) return undefined;
    if (start < text.length) parts.push(text.slice(start));
    return parts;
  }, [entries, text]);
}

export function SnippetChip({ snippet, draft = false }: { snippet: Snippet; draft?: boolean }) {
  return <span className="prompt-snippet"><CardBox icon="snippets" variant={draft ? 'composer' : 'sent'} title={snippet.key}
    subtitle="Snippet" label={`Preview snippet ${snippet.key}`} interactive previewSize={{ width: 440, height: 320 }}
    preview={<div className="prompt-snippet-preview"><strong>{snippet.key}</strong><p>{snippet.body}</p></div>}/></span>;
}
