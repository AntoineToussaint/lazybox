const decisions = document.getElementById('decisions');
const next = document.getElementById('next');
const draft = document.getElementById('draft');
let selected;

const label = text => text.replace(/[\r\n]+/g, ' ').replace(/[\\`*_{}\[\]<>()#!|]/g, '\\$&');
function githubLink(pull) {
  try {
    const url = new URL(pull.url);
    return url.origin === 'https://github.com' && !url.username && !url.password ? url.href : '';
  } catch { return ''; }
}

let workspaceId = '';
const excluded = new Set();

function sourceRow(key, title, detail) {
  const toggle = workflow.button('Include', () => {
    if (excluded.has(key)) excluded.delete(key);
    else excluded.add(key);
    updateSelection();
  });
  function updateSelection() {
    toggle.textContent = excluded.has(key) ? 'Include' : 'Included';
    toggle.setAttribute('aria-pressed', String(!excluded.has(key)));
    toggle.setAttribute('aria-label', 'Include ' + title);
  }
  updateSelection();
  return workflow.row(key, title, detail, toggle);
}

workflow.poll(10000, async () => {
  const workspace = await workflow.workspace();
  if (workspaceId !== (workspace?.id || '')) {
    workspaceId = workspace?.id || '';
    selected = undefined;
    draft.disabled = true;
    excluded.clear();
    decisions.value = '';
    next.value = '';
    if (workspace) workflow.list([], 'Loading workspace sources…');
  }
  if (!workspace) return;
  const [{ pulls }, { artifacts }] = await Promise.all([lazybox.pulls.list(workspace.id), lazybox.artifacts.list(workspace.id)]);
  selected = { workspace, pulls, artifacts };
  workflow.list([
    ...pulls.map(pull => sourceRow('pull:' + pull.key, pull.title, [pull.repository + ' #' + pull.number, pull.detail].filter(Boolean).join(' · '))),
    ...artifacts.map(artifact => sourceRow('artifact:' + artifact.id, artifact.title, 'Artifact · ' + artifact.kind)),
  ], 'No tracked pull requests or artifacts in this workspace.');
  draft.disabled = false;
});

draft.addEventListener('click', async () => {
  if (!selected) return;
  const { workspace, pulls, artifacts } = selected;
  const sections = [];
  if (next.value.trim()) sections.push('## Next step', next.value.trim());
  if (decisions.value.trim()) sections.push('## Open decisions', decisions.value.trim());
  const pullLines = pulls.filter(pull => !excluded.has('pull:' + pull.key)).map(pull => {
    const title = label(pull.repository + ' #' + pull.number + ' · ' + pull.title);
    const url = githubLink(pull);
    const detail = pull.detail ? ' — ' + label(pull.detail) : '';
    return '- ' + (url ? '[' + title + '](<' + url + '>)' : title) + detail;
  });
  const artifactLines = artifacts.filter(artifact => !excluded.has('artifact:' + artifact.id))
    .map(artifact => '- ' + label(artifact.title) + ' · ' + label(artifact.id));
  if (pullLines.length) sections.push('## Pull requests', pullLines.join('\n'));
  if (artifactLines.length) sections.push('## Artifacts', artifactLines.join('\n'));
  if (!sections.length) { workflow.say('Include a source or write a next step or decision.'); return; }
  const content = ['# Handoff · ' + label(workspace.name), ...sections].join('\n\n');
  if (content.length > 12000) { workflow.say('This draft is too long. Include fewer sources or shorten your notes.'); return; }
  try { await lazybox.artifacts.draft(workspace.id, content); }
  catch (error) { workflow.say(error.message); }
});
