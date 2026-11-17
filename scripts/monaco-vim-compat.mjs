// Explicit compatibility stopgap for monaco-vim 0.4.4 on monaco-editor 0.57.
// monaco-vim imports "monaco-editor/esm/vs/editor/editor.api" and
// "monaco-editor/esm/vs/editor/common/commands/shiftCommand", but since 0.55
// monaco-editor's exports map serves "./*" from "./esm/vs/*.js", so those
// paths no longer resolve and Vim editing in the file editor fails to load.
// This maps the old deep paths onto the exported ones. Remove it once
// monaco-vim imports exported paths (brijeshb42/monaco-vim).
export function monacoVimCompat() {
  return {
    name: 'lazybox:monaco-vim-compat',
    config: () => ({ resolve: { alias: [{ find: /^monaco-editor\/esm\/vs\/(.*)$/, replacement: 'monaco-editor/$1' }] } }),
  };
}
