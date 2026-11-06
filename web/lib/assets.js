// SPDX-License-Identifier: MIT
// The packages this page carries: the stage, and m-bootloader itself.
import { parsePackage } from "./formats.js";

/** Which build a firmware's model takes. */
const VARIANTS = {
  "FM-1": "fm1", MiniFM: "fm1", "SMK-37 Pro": "smk37-pro", "SMK-37+VA": "smk37-pro", "SMK-37 Elite": "smk37-elite",
};

export const variantOf = (model) => VARIANTS[model] ?? null;

let manifest = null;

export async function loadManifest() {
  if (!manifest) {
    const response = await fetch(new URL("../assets/manifest.json", import.meta.url));
    if (!response.ok) throw new Error("this page is missing its manifest");
    manifest = await response.json();
  }
  return manifest;
}

export async function loadPackage(kind, variant) {
  const m = await loadManifest();
  const file = m[kind]?.[variant];
  if (!file) throw new Error(`this page has no ${kind} for ${variant}`);
  const response = await fetch(new URL(`../assets/${file}`, import.meta.url));
  if (!response.ok) throw new Error(`this page is missing ${file}`);
  return parsePackage(new Uint8Array(await response.arrayBuffer()));
}
