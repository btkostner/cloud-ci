# cloud-ci-docs

Public documentation site. Renders the canonical `docs/` tree at the repo root (architecture,
roadmap, design docs, ADRs) with [VitePress](https://vitepress.dev): navigation, built-in local
search, and an "Edit this page on GitHub" link on every page. This package owns only the build
tooling and deployment config — the content itself lives in `docs/` and is not duplicated here.

Any markdown link that points outside `docs/` (e.g. `../README.md`, or a directory like
`design/` that has no single overview page) is rewritten at build time to the matching
`github.com/btkostner/cloud-ci` blob/tree URL, verified against the real file on disk — see
`.vitepress/config.mts`.

## Commands

| Task | Command |
| --- | --- |
| Install (also symlinks `docs/`, see below) | `mise run //packages/cloud-ci-docs:install` |
| Dev server | `mise run //packages/cloud-ci-docs:dev` |
| Build the static site into `./dist` | `mise run //packages/cloud-ci-docs:build` |
| Preview the built site | `npm run preview` (serves `./dist`) |
| Check (lint + typecheck + build) | `mise run //packages/cloud-ci-docs:check` |
| Deploy to Cloudflare Workers Static Assets | `mise run //packages/cloud-ci-docs:deploy` |

## Why there's a `docs/` symlink here

`scripts/link-docs.mjs` (run by `npm install`'s `postinstall`) creates `./docs` as a symlink to
`../../docs`. It's not committed — regenerated on every install, like `node_modules`. It exists
because Vite's SSR build resolves bare imports (`vue/server-renderer`) by walking up from each
source file's own path looking for `node_modules`; the canonical `docs/` tree has none in its
ancestry, so without the symlink (plus `vite.resolve.preserveSymlinks` in the VitePress config)
that walk never reaches this package's `node_modules` and the build fails.

## Deployment

`wrangler.toml` configures a pure static-assets Worker (no script, no bindings) serving `./dist`.
Deploying requires a Cloudflare account and `wrangler login`; this has not been deployed, and
`mise run //packages/cloud-ci-docs:deploy` should not be run without explicit permission from
whoever owns the target Cloudflare account.
