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
 return { path, line: line && Number.isSafeInteger(line) ? line : undefined };
}
