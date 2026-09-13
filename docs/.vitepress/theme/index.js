import SharedTheme from '@skippr/vitepress-theme/theme'

function loadSkipprAnalytics() {
  if (typeof document === 'undefined' || typeof location === 'undefined') return
  const host = location.hostname
  if (host !== 'skippr.io' && !host.endsWith('.skippr.io')) return
  if (document.querySelector('script[src*="analytics-optout.js"]')) return
  const script = document.createElement('script')
  script.src = 'https://skippr.io/analytics-optout.js'
  document.head.appendChild(script)
}

export default {
  ...SharedTheme,
  enhanceApp(ctx) {
    if (SharedTheme.enhanceApp) {
      SharedTheme.enhanceApp(ctx)
    }
    loadSkipprAnalytics()
  },
}
