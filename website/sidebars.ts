import type { SidebarsConfig } from '@docusaurus/plugin-content-docs';

const sidebars: SidebarsConfig = {
  docs: [
    {
      type: 'category',
      label: 'Philosophy',
      collapsed: false,
      items: ['philosophy'],
    },
    {
      type: 'category',
      label: 'Getting Started',
      collapsed: false,
      items: ['getting-started'],
    },
    {
      type: 'category',
      label: 'Concepts',
      items: ['concepts'],
    },
    {
      type: 'category',
      label: 'Reference',
      items: ['configuration', 'commands', 'engines'],
    },
    {
      type: 'category',
      label: 'Integrations',
      items: ['plugins', 'mcp'],
    },
    {
      type: 'category',
      label: 'Examples',
      items: [
        'examples/index',
        'examples/office-home-network',
        'examples/per-repo-plugins',
        'examples/shared-memory-rust',
        'examples/precedence-walkthrough',
      ],
    },
    {
      type: 'category',
      label: 'Operations',
      items: ['troubleshooting', 'homebrew-tap-setup', 'release', 'changelog', 'maintainers'],
    },
    {
      type: 'category',
      label: 'Legal',
      items: ['licensing', 'third-party-licenses'],
    },
  ],
};

export default sidebars;
