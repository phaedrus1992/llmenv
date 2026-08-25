// Test-only stand-in for @docusaurus/useDocusaurusContext (see docusaurusLink.tsx
// for why this needs an alias instead of a real import). Mirrors the values in
// docusaurus.config.ts so component tests exercise realistic content.
export default function useDocusaurusContext() {
  return {
    siteConfig: {
      title: 'llmenv',
      tagline: 'direnv for Claude Code and other AI tools.',
    },
  };
}
