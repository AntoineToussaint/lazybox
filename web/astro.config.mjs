// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// One Astro build serves the whole public site on the apex domain:
//
//   lazybox.ai        → custom product homepage (src/pages/index.astro)
//   lazybox.ai/docs/  → Starlight docs (src/content/docs/docs/**)
//
// There is no `base` path prefix (that is only needed for
// user.github.io/<repo>/ subpath sites). The `/docs/` prefix comes from
// nesting the docs content under a `docs/` folder in the Starlight
// collection — the custom homepage at `/` wins over Starlight's root.
//
// NOTE: the domain is pinned in two more places that must stay in sync:
// public/CNAME (GitHub Pages) and the <link rel="canonical"> fallback in
// Layout.astro.
export default defineConfig({
  site: 'https://lazybox.ai',
  integrations: [
    starlight({
      title: 'lazybox',
      description: 'A reactive PR inbox in your terminal.',
      tagline: 'A reactive PR inbox in your terminal.',
      logo: { src: './src/assets/mark.svg', replacesTitle: false },
      favicon: '/favicon.svg',
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/AntoineToussaint/lazybox',
        },
      ],
      customCss: ['./src/styles/starlight.css'],
      editLink: {
        baseUrl:
          'https://github.com/AntoineToussaint/lazybox/edit/main/web/',
      },
      // The homepage lives at `/`, so send the docs "home" crumb back there.
      components: {},
      sidebar: [
        {
          label: 'Tutorials',
          items: [
            { label: 'Overview', slug: 'docs/tutorials' },
            { label: 'Quickstart', slug: 'docs/tutorials/quickstart' },
          ],
        },
        {
          label: 'How-to guides',
          items: [
            { label: 'Overview', slug: 'docs/how-to' },
            { label: 'Add a repo', slug: 'docs/how-to/add-a-repo' },
            {
              label: 'Run an agent per workspace',
              slug: 'docs/how-to/run-an-agent-per-workspace',
            },
            {
              label: 'Per-repo env and mounts',
              slug: 'docs/how-to/per-repo-env-and-mounts',
            },
            { label: 'Remote over SSH', slug: 'docs/how-to/remote-over-ssh' },
            { label: 'Mirror to Slack', slug: 'docs/how-to/mirror-to-slack' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'Overview', slug: 'docs/reference' },
            { label: 'CLI', slug: 'docs/reference/cli' },
            { label: 'Keybindings', slug: 'docs/reference/keybindings' },
            { label: 'Configuration', slug: 'docs/reference/configuration' },
          ],
        },
        {
          label: 'Explanation',
          items: [
            { label: 'Overview', slug: 'docs/explanation' },
            { label: 'Mental model', slug: 'docs/explanation/mental-model' },
            { label: 'Architecture', slug: 'docs/explanation/architecture' },
          ],
        },
      ],
    }),
  ],
});
