// The site's table of contents. Each entry names a file under docs (or the
// changelog); its title is the page's own first heading, so a renamed page
// renames its link. Every published page must appear here: the build stops
// on one that does not (missingFromSidebar), so a new guide is never
// published without a way to reach it.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import type { DefaultTheme } from 'vitepress';

/** A group of pages: the files are relative to docs. */
type Group = { text: string; files: string[]; collapsed?: boolean };

export const groups: Group[] = [
  { text: 'Start', files: ['README.md', 'getting-started.md', 'install.md', 'testers.md', 'concepts.md'] },
  { text: 'Chat and agents', files: ['guides/chat.md', 'guides/ask-lazybox.md', 'guides/activity-detail.md', 'guides/other-agents.md', 'guides/subscriptions.md'] },
  { text: 'Workspaces and layout', files: ['guides/layouts.md', 'guides/run-your-app.md', 'guides/home-checkouts.md', 'guides/agent-browser.md', 'guides/several-runners.md'] },
  { text: 'Review and GitHub', files: ['guides/review-agent-work.md', 'guides/start-from-an-issue.md'] },
  { text: 'Missions and orchestration', files: ['guides/missions.md', 'guides/orchestrator.md'] },
  { text: 'Artifacts', files: ['guides/artifacts.md'] },
  { text: 'Automate and extend', files: ['guides/automate.md', 'guides/extensions.md'] },
  { text: 'Phone and voice', files: ['guides/phone.md', 'guides/voice.md'] },
  { text: 'Settings', files: ['guides/appearance.md'] },
  { text: 'Reference', files: ['reference/keyboard-shortcuts.md', 'reference/cli.md', 'reference/configuration.md', 'reference/extensions.md', 'reference/voice-settings.md'] },
  { text: 'Help', files: ['faq.md', 'troubleshooting.md', '../CHANGELOG.md'] },
];

/** The site path of a file under docs (see rewrites in config.ts). */
export function route(file: string): string {
  if (file === 'README.md') return '/overview';
  if (file === '../CHANGELOG.md') return '/changelog';
  return '/' + file.replace(/\.md$/, '');
}

/** The page's first heading, as the app's docs viewer titles it (titleOf in docs-model.ts). */
export function titleOf(docs: string, file: string): string {
  const text = readFileSync(join(docs, file), 'utf8');
  const heading = /^#\s+(.+?)\s*#*\s*$/m.exec(text)?.[1];
  if (!heading) throw new Error(`docs/${file} has no title: its first line should be a # heading`);
  const title = heading.replace(/[*_`]/g, '');
  // Short names where the heading carries the product name.
  return file === 'README.md' ? 'Overview' : file === '../CHANGELOG.md' ? 'Changelog' : title;
}

export function sidebar(docs: string): DefaultTheme.SidebarItem[] {
  return groups.map(group => ({
    text: group.text,
    collapsed: group.collapsed,
    items: group.files.map(file => ({ text: titleOf(docs, file), link: route(file) })),
  }));
}

/** Published pages (relative to docs) that no group lists. */
export function missingFromSidebar(pages: string[]): string[] {
  const listed = new Set(groups.flatMap(group => group.files));
  return pages.filter(page => !listed.has(page));
}
