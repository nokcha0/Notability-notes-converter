// Give related browser assets one version so an old stylesheet or worker cannot
// be mixed with a newly deployed page or WebAssembly module.
import { createHash } from "node:crypto";
import { readFile, readdir, writeFile } from "node:fs/promises";
import { resolve, join } from "node:path";
import { fileURLToPath } from "node:url";

const site = fileURLToPath(new URL("./site/", import.meta.url));
const output = resolve(process.argv[2] || "web/dist");
const files = (await readdir(site)).sort();
const sources = await Promise.all(files.map((name) => readFile(join(site, name), "utf8")));
const bindingPath = join(output, "pkg/notability_web.js");
const binding = await readFile(bindingPath, "utf8");
const hash = createHash("sha256");
sources.forEach((source) => hash.update(source));
hash.update(binding);
hash.update(await readFile(join(output, "pkg/notability_web_bg.wasm")));
const version = hash.digest("hex").slice(0, 12);
const paths = files.filter((name) => /\.(js|css)$/.test(name)).flatMap((name) => [name, `./${name}`]);
paths.push("./pkg/notability_web.js", "notability_web_bg.wasm");

function versionReferences(source) {
  for (const path of paths) {
    for (const quote of ['"', "'"]) {
      source = source.replaceAll(`${quote}${path}${quote}`, `${quote}${path}?v=${version}${quote}`);
    }
  }
  return source;
}

await Promise.all(files.map((name, index) => writeFile(join(output, name), versionReferences(sources[index]))));
await writeFile(bindingPath, versionReferences(binding));
