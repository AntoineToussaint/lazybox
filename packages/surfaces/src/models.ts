/**
 * The surfaces' DOM-free half: what a conversation needs from the person
 * (attention), what a phone row may offer (row plan), what a request asks
 * (request model), how a conversation's journal folds into messages and how
 * it is read (tail first, then live), and the built-in themes' tokens.
 *
 * The iPhone app (apps/mobile, React Native) imports `@lazybox/surfaces/models`
 * and nothing else from this package, so the desktop, the phone web app and
 * the native app decide "what needs you" the same way. Every module this entry
 * reaches stays free of React, the DOM and CSS; scripts/surfaces-models.test.ts
 * walks its imports and fails on the first one that is not.
 */
export * from './attention';
export * from './remote/row-plan';
export * from './remote/phone-sections';
export { isArchivedAway, isPinned, pinnedAt } from './agents-model';
export * from './request-model';
export * from './transcript-model';
export * from './transcript-cache';
export * from './conversation-model';
export * from './agent-status';
export * from './human-error';
export * from './live-status';
export { agentTitle } from './providers';
export { describeToolBrief, summarizeRun, summarizeTurn, isFailure, kindOf, type ToolEntry, type ToolTitle } from './tool-lines';
export { isSystemEvent, systemEventView, type SystemTone } from './system-events';
export { BUILTIN_BY_ID, type ThemeDefinition } from './appearance/themes';
