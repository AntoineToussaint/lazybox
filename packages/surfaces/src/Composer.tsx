import { useEffect, useLayoutEffect, useMemo, useRef, useState, type ClipboardEvent, type DragEvent, type FormEvent, type KeyboardEvent, type ReactNode, type RefObject } from 'react';
import { Drawing } from './Drawing';
import { conversationName } from './conversation-model';
import { ComposerAttachment } from './AttachmentThumbnail';
import { ErrorBox } from './AttachmentBox';
import { fileKey, pastedFiles } from './attachment-images';
import { shouldBoxPaste } from './attachment-boxes';
import type { ComposerAttachments } from './useComposerAttachments';
import type { Session } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import type { VimMode } from './composer-vim';
import { SnippetChip, useSnippetParts } from './SnippetPrompt';
import { Icon } from './Icons';
import { leaderKeys, useVimPreferences, type Keymap } from './keyboard';
import { useComposerVim } from './useComposerVim';
import { DictationOverlay, useComposerDictation } from './voice/ComposerDictation';
import { bindings, displayBinding, isLoneModifier } from './actions';
import type { FileReference } from './view-state';

/**
 * The message box of a conversation: attached and missing files, the text
 * (with Vim editing and prompt recall), and the send, attach, snippet and
 * interrupt controls. The draft itself is owned by the chat.
 */
export function Composer({ workingTurn, textarea, fileInput, agentName, embedded, session, sessionID, viewState, keymap, limited, message, files, attachments, missingFiles, uncertainSend, handedOver, sending, canCompose, canAttach, canSend, permissionPrompt, onSubmit, onChange, onDictate, onRecallKey, onOpenPromptHistory, onRemoveMissingFile, onDropMissingFiles, onDismissHandover, onOpenSnippets, onSelectFiles, onInterrupt, interruptInBar = false, onError, slash, info }: {
  workingTurn: boolean;
  textarea: RefObject<HTMLTextAreaElement | null>;
  fileInput: RefObject<HTMLInputElement | null>;
  agentName: string;
  embedded: boolean;
  session?: Session;
  sessionID: string;
  viewState?: ViewStateStore;
  keymap: Keymap;
  /** At a subscription limit: a message waits until the conversation continues. */
  limited: boolean;
  message: string;
  files: File[];
  /** The boxes of the files: paste, undo, move, edit, error boxes (useComposerAttachments). */
  attachments: ComposerAttachments;
  missingFiles: FileReference[];
  /** The operation ID of a send that was not confirmed. */
  uncertainSend: string;
  /** Why the draft just grew (a run handed over from a terminal). */
  handedOver: string;
  sending: boolean;
  canCompose: boolean;
  canAttach: boolean;
  canSend: boolean;
  /** Shown under the message box: confirming an agent restart for a permission change. */
  permissionPrompt?: ReactNode;
  onSubmit(event: FormEvent): void;
  onChange(text: string): void;
  /** Dictated words shown before they are kept (not saved); `onChange` keeps the result. */
  onDictate?(text: string): void;
  onOpenPromptHistory?(): void;
  /** Prompt recall (↑/↓); true when it handled the key. */
  onRecallKey(event: KeyboardEvent<HTMLTextAreaElement>, vimMode?: VimMode): boolean;
  onRemoveMissingFile(index: number): void;
  /** Send without the files a restored draft could not bring back. */
  onDropMissingFiles(): void;
  onDismissHandover(): void;
  onOpenSnippets(): void;
  /** Files chosen, dropped or pasted. */
  onSelectFiles(files: FileList | File[] | null): void;
  onInterrupt(): void;
  /** The live bar above offers the right stop for the running turn: no second Interrupt here. */
  interruptInBar?: boolean;
  onError(message: string): void;
  /** Slash commands: the menu above the box, and its keys before any other. */
  slash?: { onKeyDown(event: KeyboardEvent<HTMLTextAreaElement>, vimMode?: VimMode): boolean; above: ReactNode; textareaProps: Record<string, unknown> };
  /** The agent, model and context, first on the controls' row (ComposerInfo.tsx). */
  info?: ReactNode;
}) {
  // Keep native edits in the textarea. Reapplying a controlled value on a
  // background render can replace a WebKit insert before its input handler
  // sees it. Only external edits (restore, recall, dictation, send) write back.
  const lastInput = useRef<string | undefined>(undefined);
  useLayoutEffect(() => {
    const box = textarea.current;
    if (!box || message === lastInput.current) return;
    if (box.value !== message) box.value = message;
    lastInput.current = message;
  }, [message, textarea]);
  const vimEditing = useVimPreferences(viewState).composer;
  const reservedKeys = useMemo(() => leaderKeys(keymap), [keymap]);
  const vim = useComposerVim({ textarea, enabled: vimEditing, value: message, resetKey: sessionID, reserved: reservedKeys, onSend: () => { if (canSend || missingFiles.length > 0 || !!message.trim()) textarea.current?.form?.requestSubmit(); } });
  const dictation = useComposerDictation({ textarea, store: viewState, destination: conversationName(session), value: message, enabled: canCompose && !sending, onPreview: text => (onDictate || onChange)(text), onCommit: onChange,
    onSend: () => textarea.current?.form?.requestSubmit() });
  const snippetParts = useSnippetParts(message);
  const [editingSnippet, setEditingSnippet] = useState(false);
  useEffect(() => { if (!snippetParts) setEditingSnippet(false); }, [snippetParts]);
  const folded = !!snippetParts && !editingSnippet && !dictation.active;
  const keyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (folded && ['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) setEditingSnippet(true);
    const mod = event.metaKey || event.ctrlKey;
    // ⌘Z right after a paste became a box puts the text back where it was pasted.
    if (mod && !event.shiftKey && !event.altKey && event.key.toLowerCase() === 'z' && attachments.undoPaste()) { event.preventDefault(); return; }
    if (slash?.onKeyDown(event, vim.mode)) return; if (onRecallKey(event, vim.mode)) return; vim.textareaProps.onKeyDown?.(event);
  };
  // A pasted image (a screenshot, say) attaches like a dropped file; a large
  // text paste becomes a box; a short one stays a native paste.
  const paste = (event: ClipboardEvent<HTMLTextAreaElement>) => {
    // A plain-text paste belongs to the next draft while attachments are in flight.
    if (sending && !pastedFiles(event.clipboardData).length) return;
    const pasted = pastedFiles(event.clipboardData);
    if (!pasted.length) {
      const text = event.clipboardData.getData('text/plain');
      const node = event.currentTarget;
      if (shouldBoxPaste(text) && attachments.pasteText(text, node.selectionStart, node.selectionEnd)) { event.preventDefault(); return; }
      vim.textareaProps.onPaste?.(event); return;
    }
    event.preventDefault();
    if (!canAttach) { onError('This conversation cannot accept files right now.'); return; }
    onSelectFiles(pasted);
  };
  // A box dragged onto another takes its place (Alt+← and Alt+→ do the same from the keyboard).
  const [dragged, setDragged] = useState(-1);
  const BOX_DRAG = 'application/x-lazybox-attachment';
  const boxDrag = (index: number) => ({
    draggable: !sending,
    onDragStart: (event: DragEvent<HTMLLIElement>) => { event.dataTransfer.setData(BOX_DRAG, String(index)); event.dataTransfer.effectAllowed = 'move'; setDragged(index); },
    onDragEnd: () => setDragged(-1),
    onDragOver: (event: DragEvent<HTMLLIElement>) => { if (event.dataTransfer.types.includes(BOX_DRAG)) { event.preventDefault(); event.dataTransfer.dropEffect = 'move'; } },
    onDrop: (event: DragEvent<HTMLLIElement>) => {
      if (!event.dataTransfer.types.includes(BOX_DRAG)) return;
      event.preventDefault(); event.stopPropagation();
      const from = Number(event.dataTransfer.getData(BOX_DRAG)); setDragged(-1);
      if (Number.isInteger(from) && from !== index) attachments.move(from, index);
    },
  });
  const [drawing, setDrawing] = useState<{ session: string; file?: File }>();
  const sendLabel = sending ? 'Sending…' : workingTurn ? 'Queue follow-up' : 'Send message';

  const sendKeys = bindings(keymap['chat.send']);
  const sendKey = sendKeys[0] || 'Enter';
  const newlineHint = sendKeys.includes('Shift+Enter') || bindings(keymap['chat.send-now']).includes('Shift+Enter')
    ? '' : ' · Shift+Enter for a new line';
  // While the agent works, a message queues behind its turn; Send now's key (⌘↵) stops the turn for it.
  const sendNowKey = bindings(keymap['chat.send-now'])[0];
  const busyHint = workingTurn && sendNowKey ? `${displayBinding(sendKey).join(' ')} queues for later · Send now (${displayBinding(sendNowKey).join(' ')}) prioritizes in send order` : '';
  const dictateKey = bindings(keymap['voice.dictate'])[0];
  const dictateHint = dictateKey ? `Dictate · hold ${displayBinding(dictateKey).join(' ')} to talk, tap it ${isLoneModifier(dictateKey) ? 'twice ' : ''}to keep listening` : 'Dictate';
  // Sending while listening (↵, ⌘↵, Send) waits for the last words, then sends through this same form.
  return <>{drawing?.session === sessionID && drawing && canAttach && <Drawing file={drawing.file} onMask={drawing.file && !drawing.file.name.endsWith('.excalidraw.png') ? mask => {
    if (!attachments.select([mask])) throw new Error('The edit mask exceeds the conversation’s attachment limits.');
    setDrawing(undefined);
  } : undefined} onClose={() => setDrawing(undefined)} onAttach={file => {
    const index = drawing.file?.name.endsWith('.excalidraw.png') ? files.indexOf(drawing.file) : -1;
    const accepted = index >= 0 ? attachments.replace(index, file) : attachments.select([file]);
    if (!accepted) throw new Error('This drawing exceeds the conversation’s attachment limits.');
    setDrawing(undefined);
  }}/>}<form className="composer" onSubmit={event => { if (dictation.finishAndSend()) { event.preventDefault(); return; } onSubmit(event); }}>
    {(files.length > 0 || attachments.rejected.length > 0) && <ul className="attached-files" aria-label="Attached files" aria-live="polite">
      {files.map((file, i) => <ComposerAttachment key={fileKey(file, i, files)} file={file} disabled={sending} item={{ ...boxDrag(i), ...(dragged === i ? { 'data-dragging': true } : {}) }}
        onAnnotate={canAttach ? () => setDrawing({ session: sessionID, file }) : undefined} onRemove={() => attachments.remove(i)}
        onMove={files.length > 1 ? by => attachments.move(i, Math.max(0, Math.min(files.length - 1, i + by))) : undefined}
        onReplace={next => attachments.replace(i, next)} onInline={text => attachments.inline(i, text)}/>)}
      {attachments.rejected.map((file, i) => <ErrorBox key={`rejected-${i}-${file.name}`} name={file.name} reason={file.reason} onDismiss={() => attachments.dismiss(i)}/>)}
    </ul>}
    {missingFiles.length > 0 && <div className="restore-files" role="group" aria-label="Files to attach again">
      <p>The window reloaded, so {missingFiles.length === 1 ? 'this file needs' : 'these files need'} to be attached again (or removed) before sending:</p>
      <ul>
        {missingFiles.map((file, i) => <li key={i}><span>{file.name}</span><button type="button" onClick={() => onRemoveMissingFile(i)}>Remove {file.name}</button></li>)}
      </ul>
      <div className="restore-files-actions">
        {canAttach && <button type="button" className="primary" onClick={() => fileInput.current?.click()}>Attach again…</button>}
        <button type="button" onClick={onDropMissingFiles}>{missingFiles.length === 1 ? 'Send without it' : 'Send without them'}</button>
      </div>
    </div>}
    {uncertainSend && !sending && <small className="composer-hint">The previous send was not confirmed. Retrying the unchanged message checks the same operation.</small>}
    {handedOver && <p className="composer-handover" role="status">{handedOver}<button type="button" aria-label="Dismiss" onClick={onDismissHandover}>×</button></p>}
    {slash?.above}
    {folded && <div className="snippet-draft-summary">{snippetParts.map((part, i) => typeof part === 'string'
      ? <span key={i}>{part}</span> : <SnippetChip key={i} snippet={part} draft/>)}
      <button type="button" className="text-button" onClick={() => { setEditingSnippet(true); textarea.current?.focus(); }}>Edit prompt</button>
    </div>}
    <textarea ref={textarea} data-agent-composer data-folded-snippet={folded || undefined} aria-label="Message" placeholder={dictation.active ? 'Listening… speak now' : limited ? `Message ${agentName}… (sent when the conversation continues)` : `Message ${agentName}… (/ for commands)`}
      className={dictation.active ? 'is-dictating' : undefined}
      aria-busy={dictation.active || undefined}
      onChange={e => { if (folded) setEditingSnippet(true); const text = e.currentTarget.value; lastInput.current = text; dictation.takeOver(); onChange(text); }}
      disabled={!canCompose}
      {...vim.textareaProps}
      {...slash?.textareaProps}
      onKeyDown={keyDown}
      onPaste={paste}/>
    {editingSnippet && snippetParts && <button type="button" className="text-button" onClick={() => { setEditingSnippet(false); textarea.current?.focus(); }}>Collapse snippet text</button>}
    <DictationOverlay textarea={textarea} text={message} range={dictation.live}/>
    {permissionPrompt}
    {(vim.mode || (session?.instructionsPending && !session.closed)) && <div className="composer-notices">
        {vim.mode && <span className="vim-mode-chip" data-mode={vim.mode} role="status" aria-label={`Vim ${vim.mode} mode`} title="Vim editing: Esc for normal mode, i to type, Enter in normal mode sends. Turn it off in Settings → Shortcuts.">{vim.status}</span>}
        {session?.instructionsPending && !session.closed && <span className="composer-chip" role="status" title="What Lazybox tells agents changed since this conversation started. Only what changed goes to the agent, with your next message that starts a turn.">Instructions updated</span>}
    </div>}
    <div className="row">
      {info}
      {onOpenPromptHistory && <button type="button" className="icon-button" aria-label="Prompt history" title="Prompt history · ↑ and ↓ recall sent prompts" disabled={sending} onClick={onOpenPromptHistory}><Icon name="history"/></button>}
      <button type="button" className="icon-button" aria-label="Snippets" title={`Snippets · ${keymap['snippets.open']}`} disabled={!canCompose || sending} onClick={onOpenSnippets}><Icon name="snippets"/></button>
      <button type="button" className="attachment" aria-label={embedded ? 'Attach files' : undefined} disabled={!canAttach} onClick={() => fileInput.current?.click()}>{embedded ? <><Icon name="attach"/> <span className="attachment-label">Attach</span></> : '+ Attach files'}</button>
      <button type="button" disabled={!canAttach} onClick={() => setDrawing({ session: sessionID })}>Draw</button>
      <input ref={fileInput} hidden aria-label="Attach files" type="file" multiple onChange={e => { onSelectFiles(e.target.files); e.target.value = ''; }}/>
      <small className="composer-hint keyboard-hint">{vim.mode === 'insert' ? `${busyHint || `${sendKey} to send`} · Esc for normal mode` : vim.mode ? vim.hint : busyHint || `${sendKey} to send${newlineHint}`}</small>
      {!interruptInBar && (!embedded || session?.busy) && <button type="button" onClick={onInterrupt} disabled={!session?.busy || session.closed}>Interrupt</button>}
      <div className="composer-send-actions">
        {dictation.supported && <button type="button" className={'icon-button composer-mic' + (dictation.active ? ' is-active' : '')} aria-label={dictation.active ? 'Stop dictation' : 'Dictate'} title={dictation.active ? 'Stop and keep the draft' : dictateHint} disabled={!canCompose || sending} onClick={dictation.active ? dictation.stop : dictation.start}><Icon name={dictation.active ? 'stop' : 'mic'}/></button>}
        <button className="primary send-button" aria-label={sendLabel} disabled={!canSend}>{embedded ? <Icon name="send" size={18}/> : sendLabel}</button>
      </div>
    </div>
  </form></>;
}
