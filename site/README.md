# shunt docs site

Nimbus-based documentation site for [shunt](https://github.com/pleaseai/shunt), deployed to the `shunt-docs` Cloudflare Pages project by `.github/workflows/deploy-docs.yml`.

Requires Node.js 22.12 or newer.

```sh
npm install
npm run dev
npm run build
npm run typecheck
npm run lint:docs
```

`npm run build` writes the static site to `dist/`. The deployment workflow runs `npm ci`, builds the site, and deploys `site/dist` with `wrangler pages deploy`. The site does not bundle or deploy a custom Worker.

## Built-in publishing formats

Nimbus generates the documentation outputs during the Astro build:

- Per-page Markdown twins such as `/guides/providers/index.md`
- `/llms.txt`, `/llms-full.txt`, and section-specific LLM indexes
- Open Graph images for English pages, with `/og.png` as the site-wide fallback
- Page actions for copying or opening a page's Markdown representation

## Internationalization

English source pages live directly under `src/content/docs/`. Korean, Japanese, and Simplified Chinese translations live under the `ko/`, `ja/`, and `zh-cn/` subtrees.

`src/lib/i18n.ts` is the single source of truth for locale metadata, sidebar structure, and translated navigation labels. The catch-all route emits an English fallback page for every missing translation, preserving the same page paths across all four locales.

To add a translation, create the matching file below the locale subtree. For example, translate `src/content/docs/providers/anthropic.md` into Korean at `src/content/docs/ko/providers/anthropic.md`. The real translation automatically replaces the generated fallback at `/ko/providers/anthropic/`.
