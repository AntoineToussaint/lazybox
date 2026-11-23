import { useEffect, useMemo, useRef, useState } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { ReviewRun, Session, StartReviewRequest } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { useRunnerSettings } from './RunnerSettings';
import { errorText } from './PullRequestFlow';
import { reviewerChoices, unavailableAgents, type ReviewerChoice } from './review-model';
import { useProviderAvailability } from './useProviderAvailability';
import { refreshWorkspaceReview } from './useWorkspaceReview';
import { openRelatedConversation } from './conversation-requests';

/**
 * One-click cross-agent review, the client half (GITHUB.md, "Agent review"):
 * start a review on the Runner, then show the reviewer conversation it opened
 * and the Review tray its findings land in. Every entry point (Changes,
 * a turn's ⋯ menu, the PR card, the palette, the file list) goes through
 * `useReviewLauncher`, so they choose the reviewer the same way.
 */

export type ReviewScope = { scope: 'changes'; compare?: string } | { scope: 'turn'; turn: number } | { scope: 'pr'; member: string };

// ---- Cross-panel requests -------------------------------------------------

const SHOW_CONVERSATION = 'lazybox-show-conversation';
const SHOW_REVIEW = 'lazybox-show-review';
/** `anywhere`: show it even from another workspace (an agent tree spans workspaces). */
export type ConversationRequest = { collectionId: string; sessionId: string; anywhere?: boolean };
/** Ask the workspace to show a conversation (the reviewer, once started). */
export function showConversation(request: ConversationRequest) { window.dispatchEvent(new CustomEvent<ConversationRequest>(SHOW_CONVERSATION, { detail: request })); }
/** The workspace shows conversations other panels ask for. */
export function useConversationRequests(workspaceId: string, show: (request: ConversationRequest) => void) {
  const ref = useRef(show); ref.current = show;
  useEffect(() => {
    const listener = (event: Event) => { const request = (event as CustomEvent<ConversationRequest>).detail; if (!workspaceId || request.collectionId === workspaceId || request.anywhere) ref.current(request); };
    window.addEventListener(SHOW_CONVERSATION, listener);
    return () => window.removeEventListener(SHOW_CONVERSATION, listener);
  }, [workspaceId]);
}

export type ReviewTrayRequest = { collectionId: string; reviewId?: string };
// The tray a Changes page should open with, asked for before one was mounted.
const requestedTrays = new Map<string, ReviewTrayRequest>();
/** Open the Changes page's Review tray (on one review's findings). */
export function showReview(request: ReviewTrayRequest) {
  requestedTrays.set(request.collectionId, request);
  window.dispatchEvent(new CustomEvent<ReviewTrayRequest>(SHOW_REVIEW, { detail: request }));
}
/** The workspace opens its Changes page when a review is asked for. */
export function useReviewHost(workspaceId: string, showChanges: () => void) {
  const ref = useRef(showChanges); ref.current = showChanges;
  useEffect(() => {
    const listener = (event: Event) => { if ((event as CustomEvent<ReviewTrayRequest>).detail.collectionId === workspaceId) ref.current(); };
    window.addEventListener(SHOW_REVIEW, listener);
    return () => window.removeEventListener(SHOW_REVIEW, listener);
  }, [workspaceId]);
}
/** A Changes page takes the tray request for its workspace, now or once mounted. */
export function useReviewTrayRequests(workspaceId: string, open: (request: ReviewTrayRequest) => void) {
  const ref = useRef(open); ref.current = open;
  useEffect(() => {
    const queued = requestedTrays.get(workspaceId);
    if (queued) { requestedTrays.delete(workspaceId); ref.current(queued); }
    const listener = (event: Event) => {
      const request = (event as CustomEvent<ReviewTrayRequest>).detail;
      if (request.collectionId !== workspaceId) return;
      requestedTrays.delete(workspaceId); ref.current(request);
    };
    window.addEventListener(SHOW_REVIEW, listener);
    return () => window.removeEventListener(SHOW_REVIEW, listener);
  }, [workspaceId]);
}

// The reviewer conversation as a background tab beside the work it reviews.
/** `beside`: the conversation reviewed, whose tab the review's joins. */
export type ReviewTabRequest = { collectionId: string; sessionId: string; beside: string };
/**
 * Open a reviewer conversation as a related tab (conversation-requests.ts):
 * right after the reviewed conversation's tab, behind it, without taking the
 * focus. With no workspace page to hold it (a phone, a detached chat
 * window), it is shown the old way.
 */
export function openReviewTab(request: ReviewTabRequest) {
  const target = { collectionId: request.collectionId, sessionId: request.sessionId, commandId: '' };
  if (!openRelatedConversation({ collectionId: request.collectionId, sessionId: request.beside, commandId: '' }, target, { background: true })) showConversation({ collectionId: request.collectionId, sessionId: request.sessionId });
}

// ---- Starting one -----------------------------------------------------------

/**
 * Starts a review on the Runner. Its conversation opens as a background tab
 * in the panel group of the conversation it reviews: the person stays where
 * they are and can watch it.
 */
export async function startReview(api: RunnerClient, request: Omit<StartReviewRequest, '$typeName' | '$unknown' | 'templateId' | 'requestKey'> & { templateId?: string; requestKey?: string }): Promise<{ run?: ReviewRun; session?: Session }> {
  const started = await api.startReview(request, { timeoutMs: 60000 });
  refreshWorkspaceReview(request.collectionId);
  if (started.session) openReviewTab({ collectionId: started.session.collectionId || request.collectionId, sessionId: started.session.id, beside: request.sourceSessionId });
  return started;
}

/**
 * The reviewers a conversation's work can be reviewed by, the default first
 * (the setting's subscription, else the other provider than the author's),
 * and the start itself, with what it is doing and why it failed.
 */
export function useReviewLauncher(workspaceId: string, source?: Pick<Session, 'id' | 'profileId' | 'provider'>) {
  const { api, info } = useRunner();
  // No conversation to review (a transcript on the phone, Peek): nothing to choose a reviewer for.
  const { settings } = useRunnerSettings(!!source);
  const [busy, setBusy] = useState(''), [error, setError] = useState('');
  // `busy` is this render's: two starts in one frame (a click and `/review`) both see it empty.
  const starting = useRef(false);
  const authorProvider = source?.provider || info?.profiles.find(p => p.id === source?.profileId)?.provider || '';
  // Agents that cannot review now (not installed, no account): never offered, and the default gives way.
  const availability = useProviderAvailability(!!source);
  const missing = useMemo(() => [...unavailableAgents(availability.report)].sort().join(','), [availability.report]);
  const review = settings?.review;
  const choices = useMemo(() => reviewerChoices(info?.profiles || [], authorProvider, { reviewerMode: review?.reviewerMode, reviewer: review?.reviewer, reviewerModel: review?.reviewerModel }, new Set(missing ? missing.split(',') : [])),
    [info?.profiles, authorProvider, review?.reviewerMode, review?.reviewer, review?.reviewerModel, missing]);
  const unavailable = !workspaceId ? 'Choose a workspace first' : !source ? 'No conversation in this workspace to review yet' : !choices.length ? 'No agent is available to review with' : '';
  /** `focus`: what the reviewer should look at in particular (`/review <focus>`). */
  const start = async (scope: ReviewScope, choice: ReviewerChoice | undefined = choices[0], focus = '', templateId = '', requestKey = '') => {
    if (!source || !choice || busy || starting.current) return;
    starting.current = true; setBusy(choice.key); setError('');
    try {
      // The default reviewer is left to the Runner, which applies the setting the same way and
      // records why another agent stands in; one picked for this run is named.
      const named = !choice.isDefault;
      const started = await startReview(api, { collectionId: workspaceId, sourceSessionId: source.id, scope: scope.scope, compare: scope.scope === 'changes' ? scope.compare || '' : '', turn: scope.scope === 'turn' ? scope.turn : 0, member: scope.scope === 'pr' ? scope.member : '',
        profileId: named ? choice.profileId : '', provider: named && !choice.profileId ? choice.provider : '', focus: focus.trim(), model: '', templateId, requestKey });
      if (started.run?.state === 'failed') setError(started.run.detail);
      return started;
    } catch (e) { setError(errorText(e)); }
    finally { starting.current = false; setBusy(''); }
  };
  return { choices, start, busy, error, setError, unavailable };
}
export type ReviewLauncher = ReturnType<typeof useReviewLauncher>;
