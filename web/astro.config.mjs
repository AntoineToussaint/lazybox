// @ts-check
import { defineConfig } from 'astro/config';

// The homepage lives on the apex custom domain, so there is no `base`
// path prefix (that is only needed for user.github.io/<repo>/ subpath
// sites). Update `site` if the domain changes — it also feeds the
// canonical URL and sitemap.
//
// NOTE: the matching domain is pinned in two more places that must stay
// in sync: public/CNAME (GitHub Pages) and the <link rel="canonical">
// fallback in Layout.astro.
export default defineConfig({
  site: 'https://lazybox.ai',
});
