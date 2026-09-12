# Skipprd docs

Public engineer docs for [elt.skippr.io](https://elt.skippr.io).

Markdown lives in `docs/`. VitePress is the public renderer. Publish:

```bash
npm --prefix docs ci
npm --prefix docs run build
HOST=elt DIST="$(pwd)/docs/.vitepress/dist" ../cloud/scripts/publish-product-docs.sh
```

The last command runs from a sibling `cloud` checkout with R2 credentials loaded.
