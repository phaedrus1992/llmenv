// Test-only stand-in for @docusaurus/useBaseUrl (see docusaurusLink.tsx for why
// this needs an alias instead of a real import). Identity passthrough matches
// this site's baseUrl of '/'.
export default function useBaseUrl(path: string): string {
  return path;
}
