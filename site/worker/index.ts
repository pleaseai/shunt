// Cloudflare Pages `_worker.js` entry: serves each page's `.md` twin to LLM
// crawlers and `Accept: text/markdown` requests, HTML (with a `Link`
// alternate header) to everyone else. Bundled into `dist/_worker.js` by the
// `build:worker` script.
import { createMdRouter } from '@wave-rf/cloudflare-md-router';

export default createMdRouter({ vary: true });
