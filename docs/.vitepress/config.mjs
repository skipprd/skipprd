import { defineSkipprDocs } from '@skippr/vitepress-theme'
import { engineSections, engineSrcExclude } from './engine-nav.mjs'

const sidebar = [{ text: 'Home', link: '/' }, ...engineSections]

export default defineSkipprDocs({
  name: 'Skipprd',
  hostname: 'skippr.io',
  description:
    'Self-hosted ELT engine. Describe a source and a destination in skippr.yml, then run Skipprd discover, Skipprd schema, and Skipprd sync.',
  srcDir: 'docs',
  outDir: '.vitepress/dist',
  srcExclude: engineSrcExclude,
  vite: {
    ssr: {
      noExternal: ['@skippr/vitepress-theme'],
    },
  },
  nav: [
    { text: 'Discover', link: '/cli/discover' },
    { text: 'Schema', link: '/cli/schema' },
    { text: 'Sync', link: '/cli/sync' },
    { text: 'Guides', link: 'https://skippr.io/blog/' },
    { text: 'Contact', link: 'https://skippr.io/contact' },
    { text: 'GitHub', link: 'https://github.com/skipprd/skipprd' },
  ],
  sidebar: {
    '/': sidebar,
  },
  extraThemeConfig: {
    socialLinks: [
      { icon: 'github', link: 'https://github.com/skipprd/skipprd' },
    ],
    footer: {
      message: 'This site is source-available under PolyForm Shield 1.0.0',
      copyright: `Copyright © ${new Date().getFullYear()} Skippr Ltd`,
    },
  },
})
