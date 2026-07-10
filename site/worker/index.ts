// Cloudflare Pages `_worker.js` entry: serves each page's `.md` twin to LLM
// crawlers and `Accept: text/markdown` requests, HTML (with a `Link`
// alternate header) to everyone else. Bundled into `dist/_worker.js` by the
// `build:worker` script.
//
// NOTE: Cloudflare CDN does not respect `Vary: Accept` by default.
// To prevent cache poisoning (serving HTML to bots or Markdown to browsers),
// you must configure a Cache Rule in Cloudflare to include the `Accept` header
// (or User-Agent) in the Cache Key, or bypass caching for these routes.
import { createMdRouter } from '@wave-rf/cloudflare-md-router';

export default createMdRouter({ vary: true });
