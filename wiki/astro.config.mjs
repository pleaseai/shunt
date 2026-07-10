import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import mermaid from 'astro-mermaid';

const sidebar = [
  {
    label: 'Onboarding',
    items: [
      { label: 'Overview', slug: 'onboarding' },
      { label: 'Contributor Guide', slug: 'onboarding/contributor-guide' },
      { label: 'Staff Engineer Guide', slug: 'onboarding/staff-engineer-guide' },
      { label: 'Executive Guide', slug: 'onboarding/executive-guide' },
      { label: 'Product Manager Guide', slug: 'onboarding/product-manager-guide' },
    ],
  },
  {
    label: 'Getting Started',
    items: [
      { label: 'Overview', slug: '01-getting-started/overview' },
      { label: 'Configuration', slug: '01-getting-started/configuration' },
      { label: 'xAI Grok Provider', slug: '01-getting-started/xai-provider' },
      { label: 'Operations', slug: '01-getting-started/operations' },
    ],
  },
  {
    label: 'Deep Dive',
    items: [
      { label: 'Architecture', slug: '02-deep-dive/architecture' },
      { label: 'Routing and Configuration', slug: '02-deep-dive/routing-and-configuration' },
      { label: 'Adapters and Translation', slug: '02-deep-dive/adapters-and-translation' },
      { label: 'Authentication', slug: '02-deep-dive/authentication' },
      { label: 'Testing and Quality', slug: '02-deep-dive/testing-and-quality' },
    ],
  },
];

export default defineConfig({
  site: 'https://chatbot-pf.github.io',
  base: '/shunt/',
  integrations: [
    mermaid({
      theme: 'dark',
      autoTheme: true,
    }),
    starlight({
      title: 'shunt',
      description: 'Claude Code LLM gateway documentation',
      customCss: ['./src/styles/memex.css'],
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/chatbot-pf/shunt' },
      ],
      sidebar,
    }),
  ],
});
