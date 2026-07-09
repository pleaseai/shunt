# shunt Wiki Agent Instructions

## Build & Run Commands

- Install dependencies: `npm install`
- Develop locally: `npm run dev`
- Build static site: `npm run build` (runs `astro build`)
- Preview built site: `npm run preview`
- Build output: `wiki/dist/`

## Testing

- Run `npm run build` after changing Starlight config, Mermaid diagrams, dependencies, or generated Markdown.
- Check Mermaid blocks for dark-mode colors and a following `<!-- Sources: ... -->` comment.
- Check source citation links for valid `https://github.com/chatbot-pf/shunt/blob/main/...#L...` targets.

## Documentation Structure

- `src/content/docs/index.md`: wiki landing page.
- `src/content/docs/onboarding/`: Contributor, Staff Engineer, Executive, and Product Manager guides.
- `src/content/docs/01-getting-started/`: overview, configuration, operations.
- `src/content/docs/02-deep-dive/`: architecture, routing/configuration, adapters/translation, authentication, testing.
- `llms.txt`: concise LLM-readable documentation map.
- `llms-full.txt`: full wiki content for LLM context.

## Content Conventions

- Every page needs `title` and `description` frontmatter.
- Use source-linked citations for non-trivial claims.
- Prefer tables for structured information.
- Mermaid sequence diagrams must include `autonumber`.
- Mermaid diagrams should use dark colors: `#2d333b`, `#6d5dfc`, `#e6edf3`, `#161b22`, `#30363d`, `#8b949e`.
- Do not use `<br/>` in Mermaid labels.
- Escape bare generics in prose with backticks.

## Boundaries

- ✅ Keep generated content in English.
- ✅ Keep Starlight config aligned with `astro.config.mjs` sidebar entries.
- ⚠️ Ask before deleting generated pages or changing the site generator.
- ⚠️ Ask before changing theme dependencies or Mermaid rendering strategy.
- 🚫 Never remove citations to make prose cleaner.
- 🚫 Never modify upstream Rust behavior from inside wiki-only work.
