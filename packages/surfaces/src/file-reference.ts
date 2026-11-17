export type FileLink = { path: string; line?: number };
/** Accept file-shaped references, never commands or arbitrary URL schemes. */
export function fileReference(value: string, mode: 'explicit' | 'automatic' = 'explicit'): FileLink | undefined {
 let path = value.trim();
 if (!path || /[\n\r\0]/.test(path)) return;
 const match = /(?::(\d+)(?::\d+)?|#L(\d+)(?:-L?\d+)?)$/.exec(path);
 const line = match ? Number(match[1] || match[2]) : undefined;
 if (match) path = path.slice(0, match.index);
 try { path = decodeURIComponent(path); } catch { return; }
 if (/^[a-z][a-z0-9+.-]*:/i.test(path)) return;
 if (!path || path.startsWith('#') || path.includes('?') || (!path.includes('/') && !/^(?:[^\s]+\.[a-z0-9_-]+|README|LICENSE|Dockerfile|Makefile)$/i.test(path))) return;
 // A slash alone is not a file: origin/main and feature/foo are Git refs.
 if(mode==='automatic'&&!line&&!path.startsWith('/')&&!path.endsWith('/')&&!/^(?:[^/]+\.[a-z0-9_-]+|README|LICENSE|Dockerfile|Makefile|AGENTS|CLAUDE)$/i.test(path.split('/').pop()||''))return;
 // Inline code with a dot but no folder is as often a setting or a call (`rate.limit`, `console.log`,
 // `v1.2`) as a file: only a name that ends like a file is offered as one. A path or a line number says it itself.
 if(mode==='automatic'&&!line&&!path.includes('/')&&!wellKnownFile.test(path)&&!fileExtension.test(path))return;
 return { path, line: line && Number.isSafeInteger(line) ? line : undefined };
}
const wellKnownFile=/^(?:README|LICENSE|Dockerfile|Makefile|AGENTS|CLAUDE)$/i;
const fileExtension=/\.(?:[cm]?[jt]sx?|json[c5]?|mdx?|markdown|txt|rst|go|mod|sum|rs|py|pyi|rb|java|kt|kts|swift|c|h|cc|cpp|hpp|cs|m|mm|php|ex|exs|erl|hs|lua|pl|r|dart|scala|clj|zig|nim|sql|proto|graphql|gql|css|scss|sass|less|html?|xml|svg|vue|svelte|astro|ya?ml|toml|ini|cfg|conf|env|lock|sh|bash|zsh|fish|ps1|bat|tf|tfvars|hcl|gradle|properties|plist|csv|tsv|diff|patch|png|jpe?g|gif|webp|ico|pdf|ipynb|wasm|snap|tmpl|tpl|j2)$/i;
/** Whether a name ends like a file (`main.go`, `Dockerfile`): a host such as `example.com` does not. */
export const looksLikeFileName = (name: string) => wellKnownFile.test(name) || fileExtension.test(name);
