import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig, type MarkdownEnv, type PageData } from "vitepress";

// This config lives in packages/cloud-ci-docs/.vitepress/. `docsRoot` is the
// canonical docs/ tree's real path, used for every content-reading and
// repo-relative-link computation below. `vitepressSrcDir` is a symlink to
// the same tree *inside* this package (created by scripts/link-docs.mjs on
// install): Vite's Rollup SSR build resolves bare specifiers like
// `vue/server-renderer` by walking up from each source file's own path
// looking for node_modules, and docs/ has no node_modules in its ancestry,
// so without the symlink (plus `vite.resolve.preserveSymlinks` below) that
// walk never reaches this package's node_modules and the build fails.
const here = path.dirname(fileURLToPath(import.meta.url));
const packageRoot = path.resolve(here, "..");
const repoRoot = path.resolve(packageRoot, "../..");
const docsRoot = path.join(repoRoot, "docs");
const vitepressSrcDir = path.join(packageRoot, "docs");

const repo = "btkostner/cloud-ci";
const githubBlob = `https://github.com/${repo}/blob/main`;
const githubTree = `https://github.com/${repo}/tree/main`;

function readFirstHeading(absPath: string): string {
  const text = fs.readFileSync(absPath, "utf8");
  const match = text.match(/^#\s+(.+)$/m);
  const heading = match?.[1];
  if (heading === undefined) {
    throw new Error(`No top-level "# Heading" found in ${absPath}`);
  }
  return heading.trim();
}

function readAdrStatus(absPath: string): string {
  const text = fs.readFileSync(absPath, "utf8");
  const match = text.match(/^-\s*Status:\s*(.+)$/m);
  const status = match?.[1];
  if (status === undefined) {
    throw new Error(`No "- Status: ..." line found in ${absPath}`);
  }
  // Strip trailing markdown links, e.g. "Superseded by [0009](./...)" -> "Superseded by 0009".
  return status.replace(/\[(.+?)\]\([^)]*\)/g, "$1").trim();
}

function readDesignStatus(absPath: string): string {
  const text = fs.readFileSync(absPath, "utf8");
  // The status line is sometimes a full sentence trailing into prose (e.g.
  // "Status: Proposed. [ADR 0009](...), which this design\nimplements, is
  // **Accepted**..."); only the first bare word right after "Status:" is a
  // short, stable label — the rest is explanatory text, not part of it.
  const match = text.match(/^Status:\s*\*{0,2}([A-Za-z]+)/m);
  const status = match?.[1];
  if (status === undefined) {
    throw new Error(`No "Status: ..." line found in ${absPath}`);
  }
  return status;
}

function listMarkdown(dirAbs: string): string[] {
  return fs
    .readdirSync(dirAbs)
    .filter((name) => name.endsWith(".md") && name !== "README.md")
    .sort();
}

const adrDir = path.join(docsRoot, "adr");
const adrItems = listMarkdown(adrDir).map((file) => {
  const abs = path.join(adrDir, file);
  const slug = file.replace(/\.md$/, "");
  const number = slug.slice(0, 4);
  const title = readFirstHeading(abs).replace(/^\d+:\s*/, "");
  const status = readAdrStatus(abs);
  return { text: `${number} · ${title} (${status})`, link: `/adr/${slug}` };
});

const designDir = path.join(docsRoot, "design");
const designItems = listMarkdown(designDir).map((file) => {
  const abs = path.join(designDir, file);
  const slug = file.replace(/\.md$/, "");
  const title = readFirstHeading(abs);
  const status = readDesignStatus(abs);
  return { text: `${title} (${status})`, link: `/design/${slug}` };
});

const firstDesignItem = designItems[0];
if (!firstDesignItem) {
  throw new Error(`No design docs found in ${designDir}`);
}

/**
 * Rewrites a markdown link's href so that targets outside the rendered docs/
 * tree resolve to a GitHub blob/tree URL instead of a broken relative path,
 * since the static site ships only docs/, not the whole repository. Targets
 * inside docs/ are left untouched for VitePress's own router and dead-link
 * check to handle. Throws on a target that doesn't exist on disk, rather
 * than silently leaving a dead link or guessing.
 */
function resolveOutboundHref(currentRelativePath: string, href: string): string | null {
  if (!href) return null;
  if (/^[a-z][a-z0-9+.-]*:/i.test(href)) return null; // already absolute (http:, mailto:, ...)
  if (href.startsWith("#")) return null; // same-page anchor
  const [hrefPath, hash] = href.split("#");
  if (!hrefPath) return null;
  const currentDir = path.dirname(path.join(docsRoot, currentRelativePath));
  const targetAbs = path.resolve(currentDir, hrefPath);
  const relToDocsRoot = path.relative(docsRoot, targetAbs);
  const isOutsideDocs = relToDocsRoot.startsWith("..") || path.isAbsolute(relToDocsRoot);
  if (!isOutsideDocs) {
    // A directory inside docs/ with no index.md/README.md of its own (e.g.
    // design/, which has no single overview page by design) has no VitePress
    // route to resolve to. A link to it is still real — just not a page this
    // site renders — so it goes to the directory on GitHub instead of
    // tripping VitePress's dead-link check.
    const isUnindexedDir =
      fs.existsSync(targetAbs) &&
      fs.statSync(targetAbs).isDirectory() &&
      !fs.existsSync(path.join(targetAbs, "index.md")) &&
      !fs.existsSync(path.join(targetAbs, "README.md"));
    if (!isUnindexedDir) return null;
  }
  const relToRepoRoot = path.relative(repoRoot, targetAbs);
  if (relToRepoRoot.startsWith("..")) {
    throw new Error(
      `Outbound link escapes the repository: "${href}" linked from docs/${currentRelativePath}`,
    );
  }
  if (!fs.existsSync(targetAbs)) {
    throw new Error(
      `Outbound link target does not exist: "${href}" linked from docs/${currentRelativePath} ` +
        `(resolved to ${targetAbs})`,
    );
  }
  const isDir = fs.statSync(targetAbs).isDirectory();
  const urlPath = relToRepoRoot.split(path.sep).join("/");
  const base = isDir ? githubTree : githubBlob;
  return `${base}/${urlPath}${hash ? `#${hash}` : ""}`;
}

export default defineConfig({
  title: "cloud-ci",
  description: "CI that runs on your own Cloudflare account, with first-class GitHub support.",
  srcDir: vitepressSrcDir,
  outDir: path.join(packageRoot, "dist"),
  cacheDir: path.join(packageRoot, ".vitepress-cache"),
  cleanUrls: true,
  lastUpdated: false,
  // adr/README.md is this project's ADR index (a real page with real
  // content); VitePress only auto-routes index.md as a directory's root, so
  // this maps the existing file onto that route instead of adding one.
  rewrites: {
    "adr/README.md": "adr/index.md",
  },
  // The symlink above must resolve as itself, not its realpath, so Vite's
  // own node_modules walk (for `vue/server-renderer` during the SSR build)
  // starts from inside this package rather than from docs/'s real location.
  vite: {
    resolve: { preserveSymlinks: true },
  },
  head: [["link", { rel: "icon", href: "data:," }]],

  themeConfig: {
    nav: [
      { text: "Architecture", link: "/architecture" },
      { text: "Roadmap", link: "/roadmap" },
      { text: "Design docs", link: firstDesignItem.link },
      { text: "ADRs", link: "/adr/" },
      { text: "GitHub", link: `https://github.com/${repo}` },
    ],
    sidebar: [
      {
        text: "Overview",
        items: [
          { text: "Architecture", link: "/architecture" },
          { text: "Roadmap", link: "/roadmap" },
        ],
      },
      { text: "Design docs", items: designItems },
      { text: "Decision records", items: [{ text: "Index", link: "/adr/" }, ...adrItems] },
    ],
    search: { provider: "local" },
    socialLinks: [{ icon: "github", link: `https://github.com/${repo}` }],
    // VitePress serializes themeConfig function values to a source string
    // and re-evaluates them in an isolated scope on the client, so this
    // closes over nothing from the surrounding module — it must be a
    // string literal built entirely from its own parameter.
    // `filePath` (not `relativePath`) tracks the on-disk file through the
    // adr/README.md -> adr/index.md rewrite above, so this still points at
    // the real file on GitHub rather than a route that doesn't exist there.
    editLink: {
      pattern: (payload: PageData) =>
        `https://github.com/btkostner/cloud-ci/blob/main/docs/${payload.filePath}`,
      text: "Edit this page on GitHub",
    },
  },

  // The canonical docs/ tree uses literal `<details>`/`<sub>` tags only as
  // illustrative markdown mockups inside fenced code blocks (e.g.
  // design/pr-comment.md's "Comment layout" examples), never as real HTML
  // meant to render. Keeping markdown-it's raw-HTML passthrough off means a
  // stray, unescaped angle bracket elsewhere in prose (for example from a
  // malformed inline-code backslash escape, which CommonMark does not
  // support inside code spans) is rendered as harmless escaped text instead
  // of reaching Vue's template compiler as an unterminated tag.
  markdown: {
    html: false,
    config(md) {
      // Inline code spans (e.g. the GitHub Actions `${{ job.status }}`
      // syntax quoted in design/byo-ci.md) can contain literal `{{ }}`.
      // VitePress compiles each page's full rendered HTML as a Vue
      // template, where `{{ }}` in any element's text is a real mustache
      // interpolation unless that element is `v-pre`. Fenced code blocks
      // already get `v-pre` from VitePress's own highlighter wrapper;
      // inline spans don't, so this adds it here — same escaping, same
      // visible text, just not evaluated as a Vue expression.
      const defaultCodeInline =
        md.renderer.rules.code_inline ??
        ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
      md.renderer.rules.code_inline = (tokens, idx, options, env, self) => {
        tokens[idx]?.attrSet("v-pre", "");
        return defaultCodeInline(tokens, idx, options, env, self);
      };

      const defaultRender =
        md.renderer.rules.link_open ??
        ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
      md.renderer.rules.link_open = (tokens, idx, options, env: MarkdownEnv, self) => {
        const token = tokens[idx];
        if (token) {
          const hrefIndex = token.attrIndex("href");
          const attr = hrefIndex >= 0 ? token.attrs?.[hrefIndex] : undefined;
          if (attr) {
            const resolved = resolveOutboundHref(env.relativePath, attr[1]);
            if (resolved) {
              attr[1] = resolved;
              token.attrSet("target", "_blank");
              token.attrSet("rel", "noreferrer");
            }
          }
        }
        return defaultRender(tokens, idx, options, env, self);
      };
    },
  },
});
