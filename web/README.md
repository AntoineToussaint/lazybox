# lazybox — website

The product homepage **and** the docs for lazybox, built with
[Astro 6](https://astro.build) and [Starlight](https://starlight.astro.build).
One project, one build, one GitHub Pages site under a single domain:

| URL | Source | Built by |
|-----|--------|----------|
| `lazybox.ai` | `src/pages/index.astro` (custom homepage) | Astro |
| `lazybox.ai/docs/` | `src/content/docs/docs/**` (Diátaxis docs) | Starlight |

The `/docs/` prefix comes from nesting the Starlight content under a `docs/`
folder in the collection; the custom homepage at `/` wins over Starlight's
root.

## Develop

```bash
cd web
npm install
npm run dev      # http://localhost:4321  (homepage + /docs together)
npm run build    # → web/dist
npm run preview  # serve the build locally
```

Requires **Node 22+** (Astro 6 dropped Node 18/20).

## Layout

```
web/
  src/
    pages/index.astro          # the product homepage
    layouts/Layout.astro        # <head>/meta for the homepage
    styles/global.css           # homepage design system
    styles/starlight.css        # docs theme (brand palette over Starlight)
    assets/mark.svg             # logo mark used in the docs sidebar
    content/docs/docs/**        # the docs (tutorials/how-to/reference/explanation)
    content.config.ts           # Starlight content collection
  public/
    CNAME                       # GitHub Pages custom domain (lazybox.ai)
    favicon.svg
    llms.txt                    # served at /llms.txt
    demo/                       # product demo media used by the hero
  astro.config.mjs              # site URL + Starlight integration (sidebar, theme)
```

To add or edit docs, drop Markdown/MDX under `src/content/docs/docs/` with a
`title` (and ideally `description`) in frontmatter, and add it to the `sidebar`
in `astro.config.mjs`.

## Domain (`lazybox.ai`)

The domain is pinned in two files — keep them in sync if it changes:

| File | What |
|------|------|
| `astro.config.mjs` | `site:` — canonical URL + sitemap |
| `public/CNAME` | GitHub Pages custom domain |
| `src/layouts/Layout.astro` | canonical/OG fallback for `astro dev` |

## Deploy

One workflow — `.github/workflows/pages.yml` — runs a single `astro build`
(homepage at `/`, docs at `/docs/`) and publishes the artifact to GitHub Pages
on push to `main`. No subdomain, no second repo, no Python toolchain.
One-time: repo **Settings → Pages → Source: "GitHub Actions"**.

### DNS (apex only)

| Record | Host | Value |
|--------|------|-------|
| A | `@` | `185.199.108.153`, `.109.153`, `.110.153`, `.111.153` |
| AAAA *(optional)* | `@` | GitHub Pages IPv6 (`2606:50c0:8000–8003::153`) |
