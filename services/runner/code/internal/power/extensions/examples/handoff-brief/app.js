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

workflow.poll(10000, async () => {
  const workspace = await workflow.workspace();
  draft.disabled = true;
  if (selected?.workspace.id !== workspace?.id) {
    decisions.value = '';
    next.value = '';
  }
  selected = undefined;
  if (!workspace) return;
  const [{ pulls }, { artifacts }] = await Promise.all([lazybox.pulls.list(workspace.id), lazybox.artifacts.list(workspace.id)]);
  selected = { workspace, pulls, artifacts };
  workflow.list([
    ...pulls.map(pull => workflow.row(pull.key, pull.title, `${pull.repository} #${pull.number} · ${pull.detail}`)),
    ...artifacts.map(artifact => workflow.row(artifact.id, artifact.title, `Artifact · ${artifact.kind}`)),
  ], 'No tracked pull requests or artifacts in this workspace.');
  draft.disabled = false;
});

draft.addEventListener('click', async () => {
  if (!selected) return;
  const { workspace, pulls, artifacts } = selected;
  const content = [
    `# Handoff · ${label(workspace.name)}`,
    '## Pull requests',
    ...pulls.slice(0, 20).map(pull => {
      const title = label(`${pull.repository} #${pull.number} · ${pull.title}`);
      const url = githubLink(pull);
      return url ? `- [${title}](<${url}>)` : `- ${title}`;
    }),
    '## Artifacts',
    ...artifacts.slice(0, 20).map(artifact => `- ${label(artifact.title)} · ${artifact.id}`),
    '## Open decisions', decisions.value.trim(),
    '## Next step', next.value.trim(),
  ].join('\n\n');
  try { await lazybox.artifacts.draft(workspace.id, content); }
  catch (error) { workflow.say(error.message); }
});
