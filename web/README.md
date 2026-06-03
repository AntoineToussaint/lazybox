# lazybox — homepage

Marketing/product homepage for lazybox, built with [Astro 6](https://astro.build).
Static, zero client-side JS. The docs site is separate (`docs-site/`, MkDocs).

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

GitHub Actions → GitHub Pages, via `.github/workflows/web.yml` (push to
`main`, scoped to `web/**`). One-time: repo **Settings → Pages → Source:
"GitHub Actions"**.

> ⚠️ **One Pages site per repo.** This repo also has `docs.yml` deploying the
> MkDocs docs. Both target Pages, so they collide. Before going live, pick one:
>
> 1. **Recommended:** move `web/` to its own repo (`lazybox-web`) on the apex
>    domain `lazybox.ai`; keep docs here on `docs.lazybox.ai`.
> 2. Or merge both into one artifact: homepage at `/`, docs at `/docs/`.

### DNS (apex + docs subdomain)

| Record | Host | Value |
|--------|------|-------|
| A / ALIAS | `@` | GitHub Pages IPs (185.199.108–111.153) |
| CNAME | `docs` | `antoinetoussaint.github.io` |
