// Vite's Rollup SSR build resolves bare specifiers (e.g. `vue/server-renderer`)
// from each source file's own real path, walking up for `node_modules`. The
// canonical docs/ tree lives outside this package, so without this symlink
// (and `vite.resolve.preserveSymlinks` in .vitepress/config.mts) that walk
// never reaches this package's node_modules and the build fails. Not
// committed — regenerated here on every install, like node_modules itself.
import { existsSync, lstatSync, symlinkSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const linkPath = path.join(packageRoot, "docs");

if (existsSync(linkPath)) {
  if (!lstatSync(linkPath).isSymbolicLink()) {
    throw new Error(`${linkPath} exists and is not a symlink; refusing to overwrite`);
  }
} else {
  symlinkSync("../../docs", linkPath, "dir");
}
