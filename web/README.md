# lazybox — homepage

Marketing/product homepage for lazybox, built with [Astro 6](https://astro.build).
Static, zero client-side JS.

The homepage and the MkDocs docs (`docs-site/`) ship as **one** GitHub Pages
site under a single domain:

| URL | Source | Build output |
|-----|--------|--------------|
| `lazybox.ai` | `web/` (Astro) | `dist/` root |
| `lazybox.ai/docs/` | `docs-site/` (MkDocs) | `dist/docs/` |

## Develop

```bash
cd web
npm install
npm run dev      # http://localhost:4321
npm run build    # → web/dist
npm run preview  # serve the build locally
```

Requires **Node 22+** (Astro 6 dropped Node 18/20).

## Domain (placeholder: `lazybox.ai`)

The domain is pinned in three files — keep them in sync if it changes:

| File | What |
|------|------|
| `astro.config.mjs` | `site:` — canonical URL + sitemap |
| `public/CNAME` | GitHub Pages custom domain |
| `src/layouts/Layout.astro` | canonical fallback for `astro dev` |

## Deploy

One workflow — `.github/workflows/pages.yml` — builds **both** the Astro
homepage and the MkDocs docs into a single artifact (homepage at `/`, docs at
`/docs/`) and publishes it to GitHub Pages on push to `main`. No subdomain, no
second repo. One-time: repo **Settings → Pages → Source: "GitHub Actions"**.

The single `public/CNAME` (`lazybox.ai`) claims the domain for the whole site;
the docs inherit it under `/docs/`.

### DNS (apex only)

| Record | Host | Value |
|--------|------|-------|
| A | `@` | `185.199.108.153`, `.109.153`, `.110.153`, `.111.153` |
| AAAA *(optional)* | `@` | GitHub Pages IPv6 (`2606:50c0:8000–8003::153`) |
