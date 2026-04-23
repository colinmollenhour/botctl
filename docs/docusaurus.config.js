// @ts-check

import {themes as prismThemes} from 'prism-react-renderer';

/** @type {import('@docusaurus/types').Config} */
const config = {
  title: 'botctl',
  tagline: 'Safe tmux automation for Claude Code sessions',
  favicon: 'img/favicon.svg',

  future: {
    v4: true,
  },

  url: 'https://botctl.readthedocs.io',
  baseUrl: '/en/latest/',

  organizationName: 'colinmollenhour',
  projectName: 'botctl',

  onBrokenLinks: 'throw',

  markdown: {
    mermaid: true,
  },

  plugins: [
    ['docusaurus-plugin-llms', {
      generateLLMsTxt: true,
      generateLLMsFullTxt: true,
      generateMarkdownFiles: true,
    }],
    'docusaurus-markdown-source-plugin',
  ],

  themes: ['@docusaurus/theme-mermaid'],

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  presets: [
    [
      'classic',
      /** @type {import('@docusaurus/preset-classic').Options} */
      ({
        docs: {
          sidebarPath: './sidebars.js',
          editUrl: 'https://github.com/colinmollenhour/botctl/tree/main/docs/',
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      }),
    ],
  ],

  themeConfig:
    /** @type {import('@docusaurus/preset-classic').ThemeConfig} */
    ({
      image: 'img/og-card.svg',
      colorMode: {
        defaultMode: 'dark',
        respectPrefersColorScheme: true,
      },
      navbar: {
        title: 'botctl',
        logo: {
          alt: 'botctl logo',
          src: 'img/logo.svg',
        },
        items: [
          {
            type: 'docSidebar',
            sidebarId: 'docsSidebar',
            position: 'left',
            label: 'Docs',
          },
          {
            href: 'https://github.com/colinmollenhour/botctl',
            label: 'GitHub',
            position: 'right',
          },
        ],
      },
      footer: {
        style: 'dark',
        links: [
          {
            title: 'Documentation',
            items: [
              {
                label: 'Getting Started',
                to: '/docs/getting-started',
              },
              {
                label: 'Command Reference',
                to: '/docs/command-reference',
              },
              {
                label: 'Architecture',
                to: '/docs/architecture',
              },
            ],
          },
          {
            title: 'Project',
            items: [
              {
                label: 'GitHub',
                href: 'https://github.com/colinmollenhour/botctl',
              },
              {
                label: 'Sponsor',
                href: 'https://github.com/sponsors/colinmollenhour',
              },
              {
                label: 'Read the Docs',
                href: 'https://botctl.readthedocs.io/en/latest/',
              },
            ],
          },
        ],
        copyright: `Copyright ${new Date().getFullYear()} Colin Mollenhour`,
      },
      prism: {
        theme: prismThemes.github,
        darkTheme: prismThemes.dracula,
        additionalLanguages: ['bash', 'json', 'yaml', 'sql', 'rust'],
      },
    }),
};

export default config;
