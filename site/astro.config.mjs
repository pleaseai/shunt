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
      defaultLocale: 'root',
      locales: {
        root: { label: 'English', lang: 'en' },
        ko: { label: '한국어', lang: 'ko' },
        ja: { label: '日本語', lang: 'ja' },
        'zh-cn': { label: '简体中文', lang: 'zh-CN' },
      },
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
          translations: { ko: '시작하기', ja: 'はじめに', 'zh-CN': '开始使用' },
          items: [
            {
              label: 'Why shunt',
              translations: { ko: 'shunt이란?', ja: 'shunt とは', 'zh-CN': '为什么选择 shunt' },
              slug: 'getting-started/why-shunt',
            },
            {
              label: 'Comparison',
              translations: { ko: '비교', ja: '比較', 'zh-CN': '对比' },
              slug: 'getting-started/comparison',
            },
            {
              label: 'Installation',
              translations: { ko: '설치', ja: 'インストール', 'zh-CN': '安装' },
              slug: 'getting-started/installation',
            },
            {
              label: 'Quickstart',
              translations: { ko: '빠른 시작', ja: 'クイックスタート', 'zh-CN': '快速开始' },
              slug: 'getting-started/quickstart',
            },
          ],
        },
        {
          label: 'Guides',
          translations: { ko: '가이드', ja: 'ガイド', 'zh-CN': '指南' },
          items: [
            {
              label: 'Configuration',
              translations: { ko: '설정', ja: '設定', 'zh-CN': '配置' },
              slug: 'guides/configuration',
            },
            {
              label: 'Providers',
              translations: { ko: '프로바이더', ja: 'プロバイダー', 'zh-CN': '提供方' },
              slug: 'guides/providers',
            },
            {
              label: 'ChatGPT / Codex',
              translations: { ko: 'ChatGPT / Codex', ja: 'ChatGPT / Codex', 'zh-CN': 'ChatGPT / Codex' },
              slug: 'guides/codex',
            },
            {
              label: 'xAI / Grok',
              translations: { ko: 'xAI / Grok', ja: 'xAI / Grok', 'zh-CN': 'xAI / Grok' },
              slug: 'guides/xai',
            },
            {
              label: 'Anthropic Multi-Account',
              translations: { ko: 'Anthropic 멀티 계정', ja: 'Anthropic マルチアカウント', 'zh-CN': 'Anthropic 多账户' },
              slug: 'guides/anthropic-multi-account',
            },
            {
              label: 'Admin & Remote Provisioning',
              translations: { ko: '관리자 & 원격 프로비저닝', ja: '管理とリモートプロビジョニング', 'zh-CN': '管理与远程预配' },
              slug: 'guides/admin-remote-provisioning',
            },
            {
              label: 'Connect Claude Code',
              translations: { ko: 'Claude Code 연결', ja: 'Claude Code の接続', 'zh-CN': '连接 Claude Code' },
              slug: 'guides/connect-claude-code',
            },
            {
              label: 'Model Discovery',
              translations: { ko: '모델 디스커버리', ja: 'モデルディスカバリー', 'zh-CN': '模型发现' },
              slug: 'guides/model-discovery',
            },
            {
              label: 'Effort & Context',
              translations: { ko: 'Effort와 컨텍스트', ja: 'Effort とコンテキスト', 'zh-CN': '推理强度与上下文' },
              slug: 'guides/effort-and-context',
            },
            {
              label: 'Sharing a Gateway',
              translations: { ko: '게이트웨이 공유', ja: 'ゲートウェイの共有', 'zh-CN': '共享网关' },
              slug: 'guides/shared-gateway',
            },
            {
              label: 'OpenTelemetry',
              translations: { ko: 'OpenTelemetry', ja: 'OpenTelemetry', 'zh-CN': 'OpenTelemetry' },
              slug: 'guides/opentelemetry',
            },
          ],
        },
        {
          label: 'Reference',
          translations: { ko: '레퍼런스', ja: 'リファレンス', 'zh-CN': '参考' },
          items: [
            {
              label: 'CLI',
              translations: { ko: 'CLI', ja: 'CLI', 'zh-CN': 'CLI' },
              slug: 'reference/cli',
            },
            {
              label: 'Configuration Reference',
              translations: { ko: '설정 레퍼런스', ja: '設定リファレンス', 'zh-CN': '配置参考' },
              slug: 'reference/configuration',
            },
            {
              label: 'HTTP Endpoints',
              translations: { ko: 'HTTP 엔드포인트', ja: 'HTTP エンドポイント', 'zh-CN': 'HTTP 端点' },
              slug: 'reference/endpoints',
            },
            {
              label: 'Troubleshooting',
              translations: { ko: '문제 해결', ja: 'トラブルシューティング', 'zh-CN': '故障排查' },
              slug: 'reference/troubleshooting',
            },
          ],
        },
      ],
    }),
  ],
});
