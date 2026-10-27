// Regenerate checked-in browser modules from npm's integrity-locked packages.
import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
const root = new URL("./", import.meta.url);
const output = new URL("js/vendor/", root);
await mkdir(output, { recursive: true });
for (const [source, target] of [
  ["markdown-it/dist/browser/markdown-it.esm.min.mjs", "markdown-it.mjs"],
  ["dompurify/dist/purify.es.mjs", "purify.mjs"],
]) {
  const text = await readFile(new URL(`node_modules/${source}`, root), "utf8");
  await writeFile(new URL(target, output), text.replace(/^\/\/# sourceMappingURL=.*$/gm, ""));
}
for (const [source, target] of [
  ["markdown-it/LICENSE", "markdown-it.LICENSE"],
  ["dompurify/LICENSE", "DOMPurify.LICENSE"],
  ["dompurify/LICENSE-MPL", "DOMPurify.LICENSE-MPL"],
]) await copyFile(new URL(`node_modules/${source}`, root), new URL(target, output));
