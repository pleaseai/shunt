# shunt docs site

Astro Starlight site for [shunt](https://github.com/pleaseai/shunt), deployed to
Cloudflare Pages (`shunt-docs`) by `.github/workflows/deploy-docs.yml`.

```sh
npm install
npm run dev      # local dev server
npm run build    # astro build + bundle dist/_worker.js
```

Requires Node >= 22 (`starlight-page-actions` engines constraint).

## LLM-friendly output

- `starlight-llms-txt` generates `/llms.txt`, `/llms-full.txt`, `/llms-small.txt`.
- `starlight-page-actions` emits a per-page Markdown twin (`/guides/providers.md`)
  and the "Copy Markdown" / "Open in AI" page buttons.
- `worker/index.ts` (bundled to `dist/_worker.js`) content-negotiates: LLM
  crawlers and `Accept: text/markdown` requests get the `.md` twin; browsers get
  HTML with a `Link: rel="alternate"` header pointing at it.

## Deploy prerequisite: Cloudflare cache key

The worker emits `Vary: Accept`, but Cloudflare's CDN ignores `Vary` by default.
If edge caching is ever enabled for these routes, configure a Cache Rule in the
Cloudflare dashboard that includes the `Accept` header in the cache key (or
bypass caching for the docs routes) — otherwise the first cached response per
URL is served to every client, mixing HTML and Markdown across audiences.
