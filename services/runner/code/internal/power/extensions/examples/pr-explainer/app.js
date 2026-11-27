const target = document.getElementById('target');
let workspaceId;
const explain = `Inspect the actual merge-base-to-head changes, tests, call sites, and relevant prior behavior. Record repository, base and head SHAs, and distinguish committed from working-tree changes. If the revisions cannot be read, report the missing evidence.
Publish a Lazybox artifact using report.v1 (lazybox artifact template report). Lead with who can do what now and why. Group findings by new or changed interface, bug fix, or implementation change. For each meaningful change give a concrete before/after example, the owning implementation, compatibility implications, and revision-specific evidence. For UI changes include available visual evidence; for a complex interaction show the sequence. Separate observed checks from remaining verification gaps. Include Mission criterion IDs when available. Update the same artifact on subsequent revisions and return its ID.`;
workflow.conversationActions(async workspace => {
  const data = await lazybox.pulls.list(workspace.id);
  const previous = workspace.id === workspaceId ? target.value : '';
  workspaceId = workspace.id;
  target.replaceChildren(new Option('Current workspace changes', ''));
  for (const pull of data.pulls) target.add(new Option(pull.repository + ' #' + pull.number + ' · ' + pull.title, pull.repository + '#' + pull.number));
  if ([...target.options].some(option => option.value === previous)) target.value = previous;
  if (data.error) document.getElementById('result').textContent = data.error;
}, () => [
  ['Explain as artifact', () => explain + '\nSubject: ' + (target.value || 'current workspace changes') + '. Analyze and publish the local artifact only.'],
  ['Create draft PR', () => {
    if (target.value) throw Error('Choose current workspace changes to create a draft PR.');
    return explain + '\nCreate a draft GitHub pull request for the current branch using the verified explanation as the PR body. Reuse an existing PR for this branch if present. If the repository, base, or intended commits are ambiguous, ask before publishing. Leave uncommitted files for their owner. Return the PR link and artifact ID.';
  }],
]);
