const paths: Record<string, string[]> = {
  sidebar: ['M3 4h18v16H3z', 'M9 4v16'],
  history: ['M3 11a9 9 0 1 1 2 7', 'M3 4v7h7', 'M12 7v5l3 2'],
  snippets: ['M8 3H4v18h16V7l-4-4z', 'M8 11h8', 'M8 15h6'],
  folder: ['M3 7h6l2 2h10v11H3z', 'M3 7V4h6l2 3'],
  collections: ['M4 4h6v6H4z', 'M14 4h6v6h-6z', 'M4 14h6v6H4z', 'M14 14h6v6h-6z'],
  chat: ['M4 4h16v12H9l-5 4z'],
  terminal: ['m5 7 4 5-4 5', 'M12 17h7'],
  plus: ['M12 5v14', 'M5 12h14'],
  external: ['M14 4h6v6', 'm20 4-9 9', 'M10 4H4v16h16v-6'],
  sliders: ['M4 7h6', 'M14 7h6', 'M4 17h10', 'M18 17h2', 'M10 4v6', 'M14 14v6'],
  refresh: ['M20 8a8 8 0 1 0 0 8', 'M20 3v5h-5'],
  chevron: ['m8 10 4 4 4-4'],
  branch: ['M6 3v12a4 4 0 0 0 8 0V9', 'M6 7a2 2 0 1 0 0-4 2 2 0 0 0 0 4', 'M14 9a2 2 0 1 0 0-4 2 2 0 0 0 0 4'],
  attach: ['m9 12 6-6a3 3 0 0 1 4 4L9 20a5 5 0 0 1-7-7L13 2', 'm6 15 9-9'],
  send: ['M12 19V5', 'm6 11 6-6 6 6'],
  more: ['M5 12h.01', 'M12 12h.01', 'M19 12h.01'],
};
export function Icon({ name, size = 16 }: { name: string; size?: number }) {
  return <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">{paths[name]?.map((path, i) => <path key={i} d={path}/>)}</svg>;
}
