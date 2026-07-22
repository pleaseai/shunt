import { defineConfig } from "astro/config";
import icon from "astro-icon";
import tailwindcss from "@tailwindcss/vite";
import nimbus, { defineConfig as defineNimbusConfig } from "@cloudflare/nimbus-docs";
import { tableScroll } from "@cloudflare/nimbus-docs/markdown";
import { ENGLISH_SIDEBAR_ITEMS } from "./src/lib/i18n";

const nimbusConfig = defineNimbusConfig({
  site: "https://shunt-docs.pages.dev",
  title: "shunt",
  description: "Shunt Claude Code to any model — a spec-compliant Claude Code LLM gateway.",
  locale: "en",
  github: "https://github.com/pleaseai/shunt",
  editPattern: "https://github.com/pleaseai/shunt/edit/main/site/{path}",
  socialImage: "/og.png",
  socialImageAlt: "shunt documentation preview",
  sidebar: {
    items: ENGLISH_SIDEBAR_ITEMS,
  },
});

export default defineConfig({
  output: "static",
  vite: {
    plugins: [tailwindcss()],
  },
  prefetch: {
    prefetchAll: true,
    defaultStrategy: "hover",
  },
  integrations: [
    icon(),
    nimbus(nimbusConfig, {
      rules: {
        "nimbus/frontmatter-shape": "error",
        "nimbus/internal-link": "error",
      },
      markdown: {
        hastPlugins: [tableScroll()],
      },
    }),
  ],
});
