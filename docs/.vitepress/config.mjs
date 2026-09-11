import { defineSkipprDocs } from '@skippr/vitepress-theme'
export default defineSkipprDocs({
  name: 'Skipprd',
  hostname: 'elt.skippr.io',
  description: 'Self-hosted ELT engine — skipprd discover and skipprd sync.',
  nav: [
    { text: 'Discover', link: '/cli/discover' },
    { text: 'Sync', link: '/cli/sync' },
    { text: 'Cloud ELT', link: 'https://skippr.io/elt/' },
  ],
  sidebar: {
    '/': [
      { text: 'Home', link: '/' },
      { text: 'Discover', link: '/cli/discover' },
      { text: 'Sync', link: '/cli/sync' },
      { text: 'Connectors', link: '/connectors/' },
    ],
  },
})
