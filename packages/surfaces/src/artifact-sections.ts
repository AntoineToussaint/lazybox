// Source lines let the reader group the generated fields without mistaking a
// heading written by the author inside a requirement for a new schema field.
export type ArtifactSection = { line: number; field: string; label: string };

export function specOrPlanDocument(fields: Record<string, unknown>) {
  const spec = fields.schema === 'spec.v1';
  if (!spec && fields.schema !== 'plan.v1') return;
  const layout = spec
    ? [['problem', 'Problem'], ['requirements', 'Requirements'], ['acceptanceCriteria', 'Acceptance criteria']]
    : [['goal', 'Goal'], ['steps', 'Steps'], ['completionChecks', 'Completion checks']];
  const sections: ArtifactSection[] = [];
  const parts: string[] = [];
  let line = 1;
  for (const [field, label] of layout) {
    const value = fields[field];
    if (typeof value !== 'string' && !(Array.isArray(value) && value.every(row => typeof row === 'string'))) return;
    const text = typeof value === 'string' ? value : value.map((row, index) => (field === 'steps' ? `${index + 1}. ` : '- ') + row).join('\n');
    sections.push({ field, label, line });
    const part = '## ' + label + '\n\n' + text;
    parts.push(part);
    line += part.split('\n').length + 1;
  }
  return { markdown: parts.join('\n\n'), sections };
}
