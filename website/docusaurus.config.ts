import { themes as prismThemes } from 'prism-react-renderer';
import type { Config } from '@docusaurus/types';
import type * as Preset from '@docusaurus/preset-classic';

const config: Config = {
  title: 'llmenv',
  tagline: 'direnv for Claude Code and other AI tools.',
  favicon: 'img/favicon.ico',

  url: 'https://phaedrus1992.github.io',
  baseUrl: '/llmenv/',

  organizationName: 'phaedrus1992',
  projectName: 'llmenv',

  onBrokenLinks: 'throw',
  markdown: {
    mermaid: true,
    hooks: {
      onBrokenMarkdownLinks: 'throw',
    },
  },

  themes: ['@docusaurus/theme-mermaid'],

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  plugins: [
    [
      '@easyops-cn/docusaurus-search-local',
      {
        hashed: true,
        language: ['en'],
      },
    ],
  ],

  presets: [
    [
      'classic',
      {
        docs: {
          path: './docs',
          routeBasePath: 'docs',
          sidebarPath: './sidebars.ts',
          editUrl: 'https://github.com/phaedrus1992/llmenv/edit/main/website/',
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      } satisfies Preset.Options,
    ],
  ],

  themeConfig: {
    navbar: {
      title: 'llmenv',
      items: [
        {
          type: 'docSidebar',
          sidebarId: 'docs',
          position: 'left',
          label: 'Docs',
        },
        {
          href: 'https://github.com/phaedrus1992/llmenv',
          label: 'GitHub',
          position: 'right',
        },
      ],
    },
    footer: {
      style: 'dark',
      links: [
        {
          title: 'Learn',
          items: [
            { label: 'Getting Started', to: '/docs/getting-started' },
            { label: 'Concepts', to: '/docs/concepts' },
            { label: 'Why llmenv?', to: '/docs/philosophy' },
          ],
        },
        {
          title: 'Reference',
          items: [
            { label: 'Configuration', to: '/docs/configuration' },
            { label: 'Commands', to: '/docs/commands' },
            { label: 'Plugins', to: '/docs/plugins' },
            { label: 'MCP & Memory', to: '/docs/mcp' },
          ],
        },
        {
          title: 'Project',
          items: [
            { label: 'GitHub', href: 'https://github.com/phaedrus1992/llmenv' },
            {
              label: 'Releases',
              href: 'https://github.com/phaedrus1992/llmenv/releases',
            },
          ],
        },
      ],
      copyright: `Copyright © ${new Date().getFullYear()} Phaedrus. Built with Docusaurus.`,
    },
    prism: {
      theme: prismThemes.github,
      darkTheme: prismThemes.dracula,
      additionalLanguages: ['bash', 'yaml', 'toml'],
    },
  } satisfies Preset.ThemeConfig,
};

export default config;
