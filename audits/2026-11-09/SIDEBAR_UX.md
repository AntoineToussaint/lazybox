# Workspace navigation and deletion UX audit

Scope: the reported workspace/sidebar screenshots, action menus, deletion flow,
and the surrounding workspace toolbar. This is not certification of the whole
application or the phone development journey.

## Findings and owner fixes

| Severity | Finding and evidence | Resolution |
| --- | --- | --- |
| P0 usability | Delete existed but was clipped by the sidebar's scrolling container. The old test measured menu height without hit-testing the action. A new WebKit test with a 155px panel failed before changes (`.cache/workspace-menu-before.log`). | Shared `ActionMenu` uses the browser's top layer, clamps to the viewport, and follows its trigger while the container scrolls. Used for workspace and conversation actions. |
| P1 | Row hover and nested button hover both painted backgrounds. History painted only its text-sized button, producing an unrelated selection shape. | Selected state belongs to the row; nested navigation buttons stay transparent. History uses the same full-width hit area and a muted hover state. Sidebar fills its available height. |
| P1 | Destructive dialog put the name in a variable-length heading and mixed consequences, exceptions and instructions. | Stable heading, distinct workspace subject, short consequence text, separate warnings for active sessions and dirty files. Cancel receives initial focus; no name typing. Stop/discard acknowledgements remain required when applicable. |
| P1 | Global `label { flex-direction: column }` leaked into warning checkbox rows. Confirmed in rendered screenshot. | Workspace confirmation checkbox rows explicitly own their horizontal layout. |
| P1 | At 390px the workspace toolbar's fixed-height flex row allowed labels and controls to overlap. | Context controls wrap; the primary toolbar scrolls horizontally instead of shrinking controls into one another. Singular repository label corrected. |
| P1 interaction | Initial top-layer implementation closed during Chromium's automatic scroll-to-trigger. | Scroll updates the anchor position rather than dismissing the menu. Chromium and WebKit run the same regression. |
| P2 test quality | Existing tests could pass while Delete was invisible to pointer input. | Assert `elementFromPoint` on Delete, viewport bounds, toolbar non-overlap, keyboard/outside dismissal, full-width History alignment and stable selected-row color. |

These are fixes in the UI components that own the behavior; no backend deletion,
state, subscription, or filesystem semantics changed. `ActionMenu` centralizes
positioning, focus and dismissal instead of increasing z-index on clipped content.
Deletion submits are guarded against duplicate/ineligible form submission.

## Verification

- `npm run typecheck`
- `TEST_WEBKIT=1 npm run test:workspace-actions`
- `npm run test:workspace-actions` (Chromium)
- `npm run test:sidebar-polish` (WebKit)
- `npm run test:rich-activity`
- `npm run test:dialogs`

Workspace actions exercise clean and dirty/running confirmation, cancellation,
right-click, keyboard navigation/Escape, outside click, a short scrolling panel,
and 900px/390px viewport positioning. Fixture deletion of a different workspace
retains the current chat. Screenshots of clean and dirty dialogs, menu states and
sidebar hover were inspected. No real workspace or subscription was deleted.

Dev's Vite log records HMR for the changed components/styles. Native computer-use
inspection timed out, so the rendered visual checks are isolated browser fixtures,
not a claim that the user's exact native window was visually inspected. The full
25-gate release catalog was not rerun for this UI iteration; the previously frozen
candidate predates these changes.

## Findings at the initial audit (superseded where noted below)

1. **Phone navigation is still a desktop stack.** Even without overlap, the
   header/context/sidebar consume too much of a small viewport. Replace the
   stacked panels with one active workspace surface and a dismissible workspace
   navigator. Keep repository context and outstanding agent questions visible.
   This needs a complete phone journey test, not another breakpoint-only patch.
2. **The CSS ownership problem is broader.** Global element selectors in
   `style.css`, the broad `.workspace-shell .selected` rule and ad hoc sizes/colors
   across feature styles remain. Move feature layout rules into component-owned
   styles and define shared row/control/dialog tokens before extending surfaces.
   Do not mechanically restyle every feature in this bug fix.
3. **Action vocabulary still overlaps.** New session, Quick start and the row's
   plus button are competing entry points. Repository add/manage are separate
   menu choices. Consolidate these around explicit workspace-scoped actions and
   verify discoverability with a complete create-workspace/start-agent journey.
4. **The rest of the application still needs visual QA.** Repository picker,
   chat/streaming states, file editor/diff, approvals and subscription settings
   are not audited by this report. Test them at laptop, tablet and phone sizes,
   with loading/error/empty/long-content states, before calling the UI polished.


## Implemented polish follow-up

- A component-owned shell stylesheet defines navigation colors, spacing, control
  sizes, typography and focus treatment. Removed the global `.selected` rule
  that leaked green borders into unrelated controls; command tabs, page tabs
  and changed-file selection own their state styles.
- One **New conversation** flow is shared by the header and workspace row.
  Last-settings startup is inside that flow. The dialog identifies its workspace,
  has consistent controls, and no longer inherits a viewport-height chat body.
  Secondary page/layout/window actions moved into **Page actions**.
- Phone navigation is a modal workspace drawer; the active panel occupies the
  available viewport. Panel switching and resize preserve chat DOM, draft and
  event reader. The saved desktop composition is retained. The composer remains
  on screen, the send button stays at the trailing edge, and the header no longer
  stacks a sidebar above the chat.
- Shared dialogs render through a React portal with their UI style scope. A
  resize that hides the source panel no longer hides a live modal. Nested menu
  Escape dismisses the menu first, then allows dismissal of the navigator.
- Tests now cover both desktop and mobile entry points, safe resize while a
  dialog is open, and the same startup settings behavior after consolidation.

### Final verification

All **24 deterministic UI gates passed** through the shared catalog. Receipt:
`.cache/polish-ui-gates.json`; log: `.cache/polish-ui-gates.log`. The code snapshot
was unchanged during that run; this documentation update followed the run.
TypeScript checking and the web production build passed. Gate-policy admission
checks passed. Responsive-shell checks additionally passed in Chromium; the
required gate uses WebKit. Screenshots at desktop/tablet/phone sizes were visually
inspected, including phone navigation and the conversation-start dialog.
Previews are retained under `.cache/ui-polish/`.

The user's Dev UI served the new modules after a built-in Vite configuration
restart. The Runner was not restarted and remained healthy; the saved Claude
subscription registry matched its pre-restart fingerprint. No real workspace,
repository or subscription was deleted. Working was not promoted or changed.

A long-lived Dev Vite process retained a failed lookup for a newly imported CSS
file. A fresh-server test of missing shared-package imports recovering after file
creation passes, so this is **not claimed as a watcher fix**. The normal Vite
configuration restart cleared the stale resolver state. A precise remaining
runtime investigation is to reproduce that stale lookup in a long-lived embedded
server, record watched paths and resolver-cache invalidation, and fix the owning
Vite/dev-server behavior; no upstream ticket was filed.

Physical iPhone/iPad keyboard, safe-area behavior, accessibility settings and
remote Hub connectivity are still unqualified. This is the responsive web/native
shell, not the planned Expo/Hub phone delivery milestone. The full native/core
release catalog was not rerun or promoted for this UI iteration.

## Follow-up: repository workflow polish

- Workspace creation and repository addition now keep the dialog header, name
  controls and actions outside the scrolling repository browser. Long selections
  are bounded, and small screens use larger search text and touch targets.
- Empty search results offer Clear search without discarding selections.
- The submit handler shares the button's validity guard: pressing Enter with a
  blank workspace name cannot send PrepareWorkspace.
- Workspace repository management keeps the workspace name as context, uses a
  concise Add repositories action, and explicitly reports failed detail requests
  with Retry. Adding a repository refreshes its display metadata.
- Regression coverage in `test-unified-repositories.mjs` reproduced the invalid
  Enter submission and the hidden action with 20 selected repositories before
  their fixes. It covers 320/390px layouts and repository-detail failure/retry.
- These checks use a local fake Runner. They do not alter real workspaces,
  credentials, subscriptions or running agents; physical device keyboard behavior
  remains unverified.

Validation for this follow-up: all 24 UI gates passed on an unchanged source
snapshot (`.cache/polish-round2-ui-gates.json`), TypeScript passed, and both web
and desktop frontend builds passed. The builds retain the existing large-chunk
warnings. WebKit screenshots were inspected at 390×640 and desktop size. Dev
served all three modified components successfully; the saved subscription
registry fingerprint still matched the pre-restart baseline (three Claude
subscriptions). Native/core release qualification was not repeated for these UI
changes. This evidence paragraph was added after the gate receipt completed.
