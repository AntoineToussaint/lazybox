# Public site

This directory owns the Lazybox V2 product homepage. Edit its semantic HTML and
CSS here. Screenshots and the logo come from the same files the V2 guides use.

Run `npm ci` and `npm run site:build` from the repository root. The single
Pages artifact in `.cache/site` contains the homepage at `/`, the current
VitePress documentation at `/guide/`, and the terminal-era website at
`/v1/` and its original `/docs/…` addresses. Old V2 guide addresses
redirect with their query string and fragment. `/docs/` opens the V2 guide index.

For a project Pages site use `DOCS_BASE=/lazybox/ npm run site:build`.
The workflow takes the path and absolute public URL from GitHub Pages.
`node scripts/test-site.mjs` exercises the built site in Chromium. Supply
the same `DOCS_BASE` used for the build. Install Playwright Chromium first
if it is missing. The build itself checks internal links, anchors and assets.

## Terminal archive

`legacy.tar.gz` freezes the rendered Astro/Starlight site from
[AntoineToussaint/lazybox-v1 at 28a9a73](https://github.com/AntoineToussaint/lazybox-v1/tree/28a9a73d1a44e7797d1285597257123a1db0a13f/web).
The migration moved its home and docs landing pages to `/v1/` and `/docs/v1/`,
labeled the archive and updated old repository references. All other V1
documentation paths, screenshots, videos, captions and the Pagefind index
are retained. The import corrects two dead links found by the combined audit.

The original repo remains the source for historical terminal documentation.
To replace the snapshot, first build its web source with the two archive entry
routes, then run `python3 site/import-legacy.py /path/to/web/dist` here.
The deterministic importer copies the rendered archive and excludes product,
deployment and domain files. V2 builds need neither the V1 checkout nor its
Rust generators.

## Domain cutover

The V1 repository currently owns the custom domain. Merging the source
migration leaves that configuration intact. Preview the V2 Pages artifact
under its project path first. A domain cutover requires a coordinated owner
change in GitHub Pages and verification of HTTPS and both generations' routes.
Rollback consists of returning the domain to the V1 Pages site, which this
build does not modify.

The homepage and chat guide share the drawing demo in `docs-site/public/demos/`.
With Chromium installed for Playwright and `ffmpeg` and `cwebp` on PATH,
run `node scripts/record-drawing-demo.mjs` to record it again against fixture data.
