import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://shunt-docs.pages.dev',
  integrations: [
    starlight({
      title: 'shunt',
      description: 'Shunt Claude Code to any model — a spec-compliant Claude Code LLM gateway.',
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/pleaseai/shunt' },
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
