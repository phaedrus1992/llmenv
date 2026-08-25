// Test-only stand-in for @docusaurus/useBaseUrl (see docusaurusLink.tsx for why
// this needs an alias instead of a real import). Prefixes with this site's
// real baseUrl (docusaurus.config.ts) so a regression there still shows up in
// asserted hrefs/URLs instead of being hidden by an identity stub.
const BASE_URL = '/llmenv/';

export default function useBaseUrl(path: string): string {
  return `${BASE_URL.replace(/\/$/, '')}${path}`;
}
