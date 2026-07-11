import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLlmsTxt from 'starlight-llms-txt';
import starlightPageActions from 'starlight-page-actions';

export default defineConfig({
  site: 'https://shunt-docs.pages.dev',
  integrations: [
    starlight({
      title: 'shunt',
      description: 'Shunt Claude Code to any model — a spec-compliant Claude Code LLM gateway.',
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/pleaseai/shunt' },
      ],
      plugins: [
        starlightLlmsTxt({
          projectName: 'shunt',
          description:
            'Shunt Claude Code to any model — a spec-compliant Claude Code LLM gateway.',
          optionalLinks: [
            {
              label: 'GitHub repository',
              url: 'https://github.com/pleaseai/shunt',
              description: 'Source code, issues, and releases',
            },
          ],
        }),
        // No `baseUrl` on purpose: with it set, this plugin writes its own
        // (simpler) llms.txt at build end, clobbering starlight-llms-txt's.
        // It still emits the per-page `.md` twins and the page action buttons.
        starlightPageActions(),
      ],
      editLink: {
        baseUrl: 'https://github.com/pleaseai/shunt/edit/main/site/',
      },
      sidebar: [
        {
          label: 'Getting Started',
          items: [
            { label: 'Why shunt', slug: 'getting-started/why-shunt' },
            { label: 'Installation', slug: 'getting-started/installation' },
            { label: 'Quickstart', slug: 'getting-started/quickstart' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Configuration', slug: 'guides/configuration' },
            { label: 'Providers', slug: 'guides/providers' },
            { label: 'ChatGPT / Codex', slug: 'guides/codex' },
            { label: 'Connect Claude Code', slug: 'guides/connect-claude-code' },
            { label: 'Model Discovery', slug: 'guides/model-discovery' },
            { label: 'Effort & Context', slug: 'guides/effort-and-context' },
            { label: 'Sharing a Gateway', slug: 'guides/shared-gateway' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'CLI', slug: 'reference/cli' },
            { label: 'Configuration Reference', slug: 'reference/configuration' },
            { label: 'HTTP Endpoints', slug: 'reference/endpoints' },
            { label: 'Troubleshooting', slug: 'reference/troubleshooting' },
          ],
        },
      ],
    }),
  ],
});
