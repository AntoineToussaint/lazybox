import { providerData } from '@lazybox/client-core';
import type { AgentEvent } from '@lazybox/protocol';
import type { Attention } from '../attention';
import { approvalOf, questionsOf, type Question, type Risk } from '../request-model';

/**
 * What a phone Inbox row lets the person do without opening the conversation
 * (REMOTE.md). The same request model as the computer's cards decides it, so a
 * row never offers less care than the card: a flagged command or a request
 * the provider marks "default to no" asks for confirmation, and what a row
 * cannot show safely (a settings change, a diff, several questions, a typed
 * answer) opens the conversation instead.
 */
export type RowPlan =
  | {
    kind: 'approval'; request: AgentEvent;
    /** What will run or change, as one line. */
    what: string;
    /** "Bash", "Edit", … */
    tool: string;
    risks: Risk[];
    /** Allowing asks first: flagged risks, or the provider's default-to-no. */
    confirm: boolean;
    /** Allow is offered on the row; otherwise only Open (and Deny). */
    allow: boolean;
    /** Why Allow is not on the row, when it is not. */
    openReason?: string;
  }
  | {
    kind: 'question'; request: AgentEvent; question: Question;
    /** chips: one tap answers. multi: pick several, then Send. open: answer in the conversation. */
    mode: 'chips' | 'multi' | 'open';
    /** How many questions the request asks. */
    count: number;
  }
  /** A repository or browser request: answered on the computer, where its context is. */
  | { kind: 'elsewhere'; request: AgentEvent; text: string }
  | { kind: 'none' };

const settingsTool = (tool: string) => tool === 'mcp__lazybox_settings__update_settings' || tool === 'lazybox_update_settings';
/** Options a chip can carry legibly on a phone row. */
const maxChips = 6, maxChipLength = 40;

/** Pure: the row's actions for its first waiting request, for a phone that may respond. */
export function rowPlan(row: Attention, respond: boolean): RowPlan {
  const request = row.state === 'needs-input' && !row.session.closed ? row.session.pending[0] : undefined;
  if (!request || !respond) return { kind: 'none' };
  if (request.type === 'repository_request') return { kind: 'elsewhere', request, text: 'Add the repository or decline on your computer.' };
  if (request.type === 'browser_request') return { kind: 'elsewhere', request, text: 'The agent’s browser waits for you on your computer.' };
  if (request.type === 'conversation_request') return { kind: 'elsewhere', request, text: 'Open it to start the conversation or deny it.' };
  const data = providerData(request.providerDataJson);
  if (request.type === 'question') {
    const questions = questionsOf(data);
    const question = questions[0];
    if (!question) return { kind: 'none' };
    const legible = question.options.length > 0 && question.options.length <= maxChips && question.options.every(option => option.label.length <= maxChipLength) && !question.options.some(option => option.preview);
    const mode = questions.length > 1 || !legible || question.secret ? 'open' : question.multi ? 'multi' : 'chips';
    return { kind: 'question', request, question, mode, count: questions.length };
  }
  const approval = approvalOf(data, request.text, '');
  const what = approval.command || (approval.file ? `${approval.tool} ${approval.file}${approval.added || approval.removed ? ` · +${approval.added} −${approval.removed}` : ''}` : approval.description || approval.title || approval.tool);
  const openReason = settingsTool(approval.tool) ? 'Review the settings change in the conversation.'
    : approval.acceptUnsupported ? 'Lazybox cannot grant this kind of permission from a row.'
    : approval.patch && !approval.command ? 'Read the change in the conversation before allowing it.'
    : undefined;
  return { kind: 'approval', request, what, tool: approval.tool, risks: approval.risks, confirm: approval.risks.length > 0 || approval.defaultToNo, allow: !openReason, openReason };
}

/** The answer a chip sends, in the shape the computer's question card sends. */
export function chipAnswer(question: Question, labels: string[]): Record<string, { values: string[] }> {
  return { [question.key]: { values: question.multi ? labels : labels.slice(0, 1) } };
}
