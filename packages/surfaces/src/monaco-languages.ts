import * as monaco from 'monaco-editor';

// Monaco ships no TOML mode, so pyproject.toml, Cargo.toml and lock files
// rendered as plain text. Token names reuse the vs-dark theme's colors.
monaco.languages.register({ id: 'toml', extensions: ['.toml'], filenames: ['Cargo.lock', 'poetry.lock', 'uv.lock', 'Pipfile'], aliases: ['TOML'] });
monaco.languages.setLanguageConfiguration('toml', {
 comments: { lineComment: '#' },
 brackets: [['[', ']'], ['{', '}']],
 autoClosingPairs: [{ open: '[', close: ']' }, { open: '{', close: '}' }, { open: '"', close: '"' }, { open: "'", close: "'" }],
});
monaco.languages.setMonarchTokensProvider('toml', {
 tokenizer: {
  root: [
   [/#.*$/, 'comment'],
   [/^\s*\[\[[^\]]*\]\]/, 'type'],
   [/^\s*\[[^\]]*\]/, 'type'],
   [/(?:"[^"]*"|'[^']*'|[A-Za-z0-9_-]+)(?:\s*\.\s*(?:"[^"]*"|'[^']*'|[A-Za-z0-9_-]+))*(?=\s*=)/, 'attribute.name'],
   [/"""/, 'string', '@multiline'],
   [/'''/, 'string', '@literal'],
   [/"/, 'string', '@string'],
   [/'[^']*'/, 'string'],
   [/\b(?:true|false)\b/, 'keyword'],
   [/\d{4}-\d{2}-\d{2}(?:[Tt ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-]\d{2}:\d{2})?)?/, 'number'],
   [/[+-]?(?:0x[0-9a-fA-F_]+|0o[0-7_]+|0b[01_]+|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?|inf|nan)\b/, 'number'],
   [/[{}[\]]/, '@brackets'],
   [/[=,.]/, 'delimiter'],
  ],
  string: [[/[^\\"]+/, 'string'], [/\\./, 'string.escape'], [/"/, 'string', '@pop']],
  multiline: [[/"""/, 'string', '@pop'], [/[^"\\]+/, 'string'], [/\\./, 'string.escape'], [/"/, 'string']],
  literal: [[/'''/, 'string', '@pop'], [/[^']+/, 'string'], [/'/, 'string']],
 },
});

// Plain key=value files read well with the existing ini mode.
monaco.languages.register({ id: 'ini', extensions: ['.env', '.editorconfig', '.npmrc'], filenames: ['.env', '.env.example', '.env.local'] });
