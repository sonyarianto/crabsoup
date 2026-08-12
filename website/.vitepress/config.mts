import { defineConfig } from 'vitepress'

export default defineConfig({
  title: 'Crabsoup',
  description: 'A Liquidsoap-inspired audio streaming engine in Rust',
  lang: 'en-US',
  cleanUrls: true,

  head: [
    ['meta', { name: 'theme-color', content: '#d4472a' }],
  ],

  themeConfig: {
    nav: [
      { text: 'Guide', link: '/guide/getting-started' },
      { text: 'Architecture', link: '/architecture' },
      { text: 'Roadmap', link: '/roadmap' },
      { text: 'GitHub', link: 'https://github.com/sonyarianto/crabsoup' },
    ],

    sidebar: {
      '/guide/': [
        {
          text: 'Guide',
          items: [
            { text: 'Getting started', link: '/guide/getting-started' },
            { text: 'Example script', link: '/guide/example-script' },
          ],
        },
        {
          text: 'Reference',
          items: [
            { text: 'Sources', link: '/guide/sources' },
            { text: 'DSP & metadata operators', link: '/guide/dsp' },
            { text: 'Outputs', link: '/guide/outputs' },
            { text: 'Control port', link: '/guide/control-port' },
          ],
        },
      ],
    },

    footer: {
      message: 'MIT licensed',
      copyright: 'Copyright \u00a9 2026 Crabsoup contributors',
    },

    search: {
      provider: 'local',
    },

    socialLinks: [
      { icon: 'github', link: 'https://github.com/sonyarianto/crabsoup' },
    ],
  },
})