// SPDX-License-Identifier: MIT
// The flash and package formats, as in M-Flasher's src/formats.rs. Everything here works on
// Uint8Arrays and touches nothing else, so that it can be tested against real files.

export const SECTOR = 0x1000;
export const BOOT_AREA = 0x4000;
export const BOOT_AT = 0xa0;
export const FLASH_SIZE = 0x100000;

const FWSC_BLOCK = 48;
const FWSC_KEEP = 47;
const FWSC_SENTINEL = 0x7d;

export function crc16(data, crc = 0) {
  for (let i = 0; i < data.length; i++) {
    crc ^= data[i] << 8;
    for (let b = 0; b < 8; b++) crc = crc & 0x8000 ? ((crc << 1) ^ 0x1021) & 0xffff : (crc << 1) & 0xffff;
  }
  return crc;
}

const CRC32_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let i = 0; i < 256; i++) {
    let c = i;
    for (let b = 0; b < 8; b++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[i] = c >>> 0;
  }
  return table;
})();

export function crc32(data) {
  let c = 0xffffffff;
  for (let i = 0; i < data.length; i++) c = CRC32_TABLE[(c ^ data[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

/** JieLi's ENC cipher, its own inverse. */
export function enc(data, key) {
  const out = new Uint8Array(data.length);
  key &= 0xffff;
  for (let i = 0; i < data.length; i++) {
    out[i] = data[i] ^ (key & 0xff);
    key = ((key << 1) ^ (key & 0x8000 ? 0x1021 : 0)) & 0xffff;
  }
  return out;
}

const u16 = (b, at) => b[at] | (b[at + 1] << 8);
const u32 = (b, at) => (b[at] | (b[at + 1] << 8) | (b[at + 2] << 16) | (b[at + 3] << 24)) >>> 0;
function put16(b, at, v) {
  b[at] = v & 0xff;
  b[at + 1] = (v >>> 8) & 0xff;
}
function put32(b, at, v) {
  for (let i = 0; i < 4; i++) b[at + i] = (v >>> (8 * i)) & 0xff;
}

function cstr(b) {
  let s = "";
  for (let i = 0; i < b.length && b[i]; i++) s += String.fromCharCode(b[i]);
  return s;
}

export function hex(value, width = 0) {
  return "0x" + value.toString(16).toUpperCase().padStart(width, "0");
}

export function rangesText(ranges) {
  return ranges.map(([a, b]) => `${hex(a, 5)}-${hex(b - 1, 5)}`).join(", ");
}

// ---- file tables ----

function parseEntry(raw) {
  return {
    dataCrc: u16(raw, 2),
    addr: u32(raw, 4),
    len: u32(raw, 8),
    attr: raw[12],
    name: cstr(raw.subarray(16, 32)),
    valid: crc16(raw.subarray(2, 32)) === u16(raw, 0),
  };
}

function flashHeaderValid(flash) {
  const h = enc(flash.subarray(0, 32), 0xffff);
  return crc16(h.subarray(2)) === u16(h, 0);
}

/** The boot area's file table, or empty if the flash header is damaged. */
export function bootTable(flash) {
  if (flash.length < BOOT_AREA || !flashHeaderValid(flash)) return [];
  const out = [];
  for (let a = 0x20; a < BOOT_AT; a += 0x20) {
    const e = parseEntry(enc(flash.subarray(a, a + 32), 0xffff));
    if (!e.valid) break;
    out.push(e);
  }
  return out;
}

/** n plain bytes at offset off of the app area at base. */
export function appRead(flash, base, off, n, key) {
  const first = off - (off % 32);
  const out = new Uint8Array(n + 64);
  let got = 0;
  for (let a = first; a < off + n; a += 32) {
    const start = base + a;
    if (start + 32 > flash.length) break;
    out.set(enc(flash.subarray(start, start + 32), key ^ ((a >>> 2) & 0xffff)), got);
    got += 32;
  }
  const skip = off % 32;
  return out.subarray(skip, Math.min(skip + n, got));
}

/** Files (addresses in the app area) and region descriptors (VM, BTIF, ...; absolute addresses). */
export function appTable(flash, key) {
  const out = [];
  for (let i = 0; i < 16; i++) {
    const raw = appRead(flash, BOOT_AREA, 32 * i, 32, key);
    if (raw.length < 32) break;
    const e = parseEntry(raw);
    if (!e.valid) break;
    out.push(e);
  }
  return out;
}

export const AppStatus = { OK: "OK", BAD_CRC: "BAD_CRC", INCOMPLETE: "INCOMPLETE", NOT_FOUND: "NOT_FOUND" };

export function appStatusText(status) {
  return {
    OK: "OK",
    BAD_CRC: "DAMAGED (checksum fails; it may still start)",
    INCOMPLETE: "INCOMPLETE (partly erased: an interrupted write)",
    NOT_FOUND: "none installed",
  }[status];
}

/** As the bootloader's menu reports it: a damaged file with erased sectors in it is an interrupted write. */
export function appFilesStatus(flash, key) {
  const files = appTable(flash, key).filter((e) => e.attr === 0x82);
  if (!files.length) return AppStatus.NOT_FOUND;
  for (const e of files) {
    const start = BOOT_AREA + e.addr;
    const end = start + e.len;
    if (end > flash.length) return AppStatus.BAD_CRC;
    if (crc16(appRead(flash, BOOT_AREA, e.addr, e.len, key)) !== e.dataCrc) {
      for (let s = start - (start % SECTOR); s < end; s += SECTOR) {
        if (flash.subarray(s, s + SECTOR).every((b) => b === 0xff)) return AppStatus.INCOMPLETE;
      }
      return AppStatus.BAD_CRC;
    }
  }
  return AppStatus.OK;
}

// ---- bootloader ----

/** The payload of a valid BANKCB container, or null. */
export function bankcbPayload(boot) {
  if (boot.length < 16) return null;
  const size = u16(boot, 2);
  const offset = u32(boot, 8);
  if (u16(boot, 0) !== 1 || crc16(boot.subarray(0, 14)) !== u16(boot, 14) || offset + size > boot.length) return null;
  const payload = boot.subarray(offset, offset + size);
  return crc16(payload) === u16(boot, 12) ? payload : null;
}

/** [board tag, version] of m-bootloader's identity block in a container. */
export function bootloaderIdentity(boot) {
  const payload = bankcbPayload(boot);
  if (!payload || payload.length < 0x70) return null;
  if (cstr(payload.subarray(0x40, 0x48)) !== "AC79BLID") return null;
  return [cstr(payload.subarray(0x48, 0x50)), cstr(payload.subarray(0x50, 0x70))];
}

/** The uboot.boot container stored in a boot area. */
export function storedBootloader(flash) {
  const e = bootTable(flash)[0];
  if (!e || e.name !== "uboot.boot" || e.addr !== BOOT_AT || BOOT_AT + e.len > flash.length || e.len < 16) return null;
  const stored = flash.subarray(BOOT_AT, BOOT_AT + e.len);
  const boot = new Uint8Array(e.len);
  boot.set(enc(stored.subarray(0, 16), 0xffff), 0);
  boot.set(enc(stored.subarray(16), 0xffff), 16);
  return crc16(boot) === e.dataCrc ? boot : null;
}

// ---- packages ----

/** Parses an M-UPGRADE .fwsc: its identity, and the image that hosts serve to the unit. */
export function parsePackage(raw) {
  const bad = () => new Error("not a firmware package (.fwsc)");
  if (raw.length < 0x1000) throw bad();
  let blocks = 0;
  while (blocks < 20 || raw[blocks * FWSC_BLOCK + FWSC_KEEP] === FWSC_SENTINEL) {
    blocks++;
    if ((blocks + 1) * FWSC_BLOCK > raw.length) throw bad();
  }
  let identity = "";
  for (let i = 0; i < blocks; i++) {
    const m = raw[i * FWSC_BLOCK + FWSC_KEEP];
    if (m === FWSC_SENTINEL) break;
    identity += String.fromCharCode((m - (i + 1)) & 0xff);
  }
  const image = new Uint8Array(raw.length - blocks);
  for (let i = 0; i < blocks; i++) image.set(raw.subarray(i * FWSC_BLOCK, i * FWSC_BLOCK + FWSC_KEEP), i * FWSC_KEEP);
  image.set(raw.subarray(blocks * FWSC_BLOCK), blocks * FWSC_KEEP);
  const head = enc(image.subarray(0, 0x40), 0xffff);
  if (crc16(head.subarray(2)) !== u16(head, 0)) throw new Error("not a firmware package, or a damaged one (header CRC)");
  const count = u16(head, 8);
  if (0x40 + 0x50 * count > image.length) throw new Error("damaged firmware package (file list)");
  const list = image.subarray(0x40, 0x40 + 0x50 * count);
  if (crc16(list) !== u16(head, 2)) throw new Error("damaged firmware package (file list CRC)");
  const files = {};
  for (let i = 0; i < count; i++) {
    const e = enc(list.subarray(i * 0x50, (i + 1) * 0x50), 0xffff);
    const off = u32(e, 8);
    const size = u32(e, 12);
    if (off + size > image.length) throw new Error("damaged firmware package (file outside the image)");
    files[cstr(e.subarray(0x40))] = { type: u16(e, 0), crc: u16(e, 4), data: image.subarray(off, off + size) };
  }
  const at = identity.lastIndexOf("_");
  return { identity, model: at >= 0 ? identity.slice(0, at) : identity, version: at >= 0 ? identity.slice(at + 1) : "", files, image };
}

/** The flash image from a parsed package, checked; throws when it has none. */
export function packageFlash(pkg) {
  const file = pkg.files["flash.bin"];
  if (!file || file.type !== 0) throw new Error("the package has no flash image (flash.bin)");
  if (crc16(file.data) !== file.crc) throw new Error("damaged firmware package (flash.bin CRC)");
  if (file.data.length <= BOOT_AREA || file.data.length % SECTOR) throw new Error(`unexpected flash image size ${file.data.length}`);
  return file.data;
}

/** A package a unit can install from: a header at 0, and flash.bin, the first `size` bytes of flash, at 0x400. */
export function packageImage(flash, size) {
  const OFFSET = 0x400;
  const image = new Uint8Array(OFFSET + size);
  image.fill(0xff, 0, OFFSET);
  image.set(flash.subarray(0, size), OFFSET);
  const entry = new Uint8Array(0x50);
  put16(entry, 0, 0);
  put16(entry, 2, 0);
  put16(entry, 4, crc16(flash.subarray(0, size)));
  put32(entry, 8, OFFSET);
  put32(entry, 12, size);
  put32(entry, 16, size);
  entry.set(new TextEncoder().encode("flash.bin"), 0x40);
  const stored = enc(entry, 0xffff);
  const head = new Uint8Array(0x40);
  put16(head, 2, crc16(stored));
  put32(head, 4, OFFSET + size);
  put16(head, 8, 1);
  put16(head, 0, crc16(head.subarray(2)));
  image.set(enc(head, 0xffff), 0);
  image.set(stored, 0x40);
  return image;
}

export async function sha256Hex(data) {
  const digest = await crypto.subtle.digest("SHA-256", data);
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}
