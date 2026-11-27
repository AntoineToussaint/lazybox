const decisions = document.getElementById('decisions');
const next = document.getElementById('next');
const draft = document.getElementById('draft');
const sourceStatus = document.getElementById('source-status');
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

const sourceControls = document.getElementById('source-controls');
const includeAll = document.getElementById('include-all');
const excludeAll = document.getElementById('exclude-all');

function renderSelection() {
  const rows = document.querySelectorAll('#rows > li');
  let included = 0;
  for (const row of rows) {
    const on = !excluded.has(row.dataset.key);
    const toggle = row.querySelector('button');
    toggle.textContent = on ? 'Included' : 'Include';
    toggle.setAttribute('aria-pressed', String(on));
    if (on) included++;
  }
  sourceControls.hidden = rows.length === 0;
  document.getElementById('selection-count').textContent = included + ' of ' + rows.length + ' sources selected';
  includeAll.disabled = included === rows.length;
  excludeAll.disabled = included === 0;
}

includeAll.onclick = () => { excluded.clear(); renderSelection(); };
excludeAll.onclick = () => {
  for (const row of document.querySelectorAll('#rows > li')) excluded.add(row.dataset.key);
  renderSelection();
};

function sourceRow(key, title, detail) {
  const toggle = workflow.button('Include', () => {
    if (excluded.has(key)) excluded.delete(key);
    else excluded.add(key);
    renderSelection();
  });
  toggle.setAttribute('aria-label', 'Include ' + title);
  return workflow.row(key, title, detail, toggle);
}

workflow.poll(10000, async () => {
  const workspace = await workflow.workspace();
  if (workspaceId !== (workspace?.id || '')) {
    workspaceId = workspace?.id || '';
    selected = undefined;
    draft.disabled = true;
    excluded.clear();
    sourceStatus.hidden = true;
    sourceControls.hidden = true;
    decisions.value = '';
    next.value = '';
    if (workspace) workflow.list([], 'Loading workspace sources…');
  }
  if (!workspace) return;
  const [pullResult, artifactResult] = await Promise.allSettled([lazybox.pulls.list(workspace.id), lazybox.artifacts.list(workspace.id)]);
  const pulls = pullResult.status === 'fulfilled' ? pullResult.value.pulls : selected?.pulls || [];
  const artifacts = artifactResult.status === 'fulfilled' ? artifactResult.value.artifacts : selected?.artifacts || [];
  const warnings = [];
  const pullError = pullResult.status === 'rejected' ? pullResult.reason.message : pullResult.value.error;
  if (pullError) warnings.push('Pull request tracking: ' + pullError);
  if (artifactResult.status === 'rejected') warnings.push('Could not load artifacts: ' + artifactResult.reason.message);
  if (pullResult.status === 'rejected' && pulls.length) warnings.push('Showing the last loaded pull requests.');
  if (artifactResult.status === 'rejected' && artifacts.length) warnings.push('Showing the last loaded artifacts.');
  if (pullResult.value?.truncated) warnings.push('Showing the first ' + pulls.length + ' pull requests.');
  if (artifactResult.value?.truncated) warnings.push('Showing the first ' + artifacts.length + ' artifacts.');
  sourceStatus.textContent = warnings.join(' ');
  sourceStatus.hidden = !warnings.length;
  selected = { workspace, pulls, artifacts };
  workflow.list([
    ...pulls.map(pull => sourceRow('pull:' + pull.key, pull.title, [pull.repository + ' #' + pull.number, pull.detail].filter(Boolean).join(' · '))),
    ...artifacts.map(artifact => sourceRow('artifact:' + artifact.id, artifact.title, 'Artifact · ' + artifact.kind)),
  ], warnings.length ? '' : 'No tracked pull requests or artifacts in this workspace.');
  renderSelection();
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
