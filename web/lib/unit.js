// SPDX-License-Identifier: MIT
// The devices, what a flash image says about a unit, and what may be written, as in M-Flasher's
// src/boards.rs and src/ops.rs. The web tool writes through MIDI mode, which can't touch the boot
// area or the key area, and on the SMK-37 keeps Bluetooth and calibration.
import {
  AppStatus, BOOT_AREA, FLASH_SIZE, SECTOR, appFilesStatus, appRead, appStatusText, appTable, bootTable,
  bootloaderIdentity, hex, packageFlash, parsePackage, rangesText, storedBootloader,
} from "./formats.js";

export const CHIP_KEY = 0x980f;
const KEY_MAC = [0xff000, FLASH_SIZE];

export const FM1 = {
  key: "fm1", name: "FM-1", tag: "fm1", models: ["FM-1", "MiniFM"], flashId: 0x856014, otherFlash: [0xb36014],
  factory: [KEY_MAC], keep: [],
};
export const SMK37 = {
  key: "smk37", name: "SMK-37 Pro / Elite", tag: "smk37", models: ["SMK-37 Pro", "SMK-37 Elite"], flashId: 0xeb6014, otherFlash: [],
  factory: [[0xc1000, 0xc2000], KEY_MAC], keep: [[0xc0000, 0xc2000]],
};
export const BOARDS = [FM1, SMK37];

export const flashTested = (id) => BOARDS.some((b) => b.flashId === id || b.otherFlash.includes(id));

const matches = (flash, at, text) => {
  const bytes = new TextEncoder().encode(text);
  return flash.length >= at + bytes.length && bytes.every((b, i) => flash[at + i] === b);
};

function findAscii(hay, text, from = 0) {
  const needle = new TextEncoder().encode(text);
  outer: for (let i = from; i + needle.length <= hay.length; i++) {
    if (hay[i] !== needle[0]) continue;
    for (let j = 1; j < needle.length; j++) if (hay[i + j] !== needle[j]) continue outer;
    return i;
  }
  return -1;
}

function findIdentities(text) {
  const out = [];
  for (const m of ["FM-1", "MiniFM", "SMK-37 Pro", "SMK-37 Elite", "SMK-37+VA"]) {
    const prefix = m + "_";
    for (let i = findAscii(text, prefix); i >= 0; i = findAscii(text, prefix, i + 1)) {
      const d = text.subarray(i + prefix.length, i + prefix.length + 3);
      if (d.length === 3 && d.every((c) => c >= 0x30 && c <= 0x39)) out.push([m, String.fromCharCode(...d)]);
    }
  }
  return out;
}

function slopVersion(text) {
  const prefix = "SLOOP ";
  for (let i = findAscii(text, prefix); i >= 0; i = findAscii(text, prefix, i + 1)) {
    let e = i + prefix.length;
    let v = "";
    while (e < text.length && ((text[e] >= 0x30 && text[e] <= 0x39) || text[e] === 0x2e)) v += String.fromCharCode(text[e++]);
    if (/^\d/.test(v) && v.includes(".")) return v.replace(/\.+$/, "");
  }
  return null;
}

/** What a flash image says: the model and how sure, the bootloader and the installed firmware. */
export function analyse(flash, flashId, key) {
  const evidence = [];
  const boot = storedBootloader(flash);
  const identity = boot && bootloaderIdentity(boot);
  if (identity) for (const b of BOARDS) if (b.tag === identity[0]) evidence.push([b, true, `the installed bootloader is built for the ${b.name}`]);
  if (matches(flash, 0xc1224, "trimdat\0")) evidence.push([SMK37, true, "SMK-37 factory calibration found"]);
  if (matches(flash, 0xff000, "TRIMCOPY")) evidence.push([SMK37, true, "copy of the SMK-37 calibration found"]);
  const table = appTable(flash, key);
  const region = (n) => table.find((e) => (e.attr === 0x12 || e.attr === 0x92) && e.name === n)?.addr;
  if (region("USRTRIM") === 0xc1000) evidence.push([SMK37, false, "the firmware has the SMK-37 calibration area"]);
  else if (((region("VM") === 0x93000 || region("BTIF") === 0xe9000) && region("USR") === 0xea000)
    || (region("VM") === 0xba000 && region("SEQ64") === 0xda000 && region("BTIF") === 0xec000)) {
    evidence.push([FM1, false, "the firmware uses the FM-1 flash layout"]);
  }
  if (flashId != null) for (const b of BOARDS) if (flashId === b.flashId || b.otherFlash.includes(flashId)) evidence.push([b, false, `flash chip ${hex(flashId, 6)}`]);
  const keys = (strong) => [...new Set(evidence.filter((e) => e[1] === strong).map((e) => e[0].key))];
  const strong = keys(true);
  const weak = keys(false);
  let board = null;
  let certain = false;
  if (strong.length === 1) {
    board = BOARDS.find((b) => b.key === strong[0]);
    certain = weak.every((k) => k === strong[0]);
  } else if (!strong.length && weak.length === 1) {
    board = BOARDS.find((b) => b.key === weak[0]);
    certain = evidence.filter((e) => e[0].key === weak[0]).length >= 2;
  }

  let bootloaderKind;
  let bootloaderText;
  if (!boot) {
    bootloaderKind = bootTable(flash).length ? "unknown" : "damaged";
    bootloaderText = bootTable(flash).length ? "unreadable" : "damaged or missing";
  } else if (identity) {
    bootloaderKind = "open";
    const named = /^\d/.test(identity[1]) && identity[1].includes(".") ? "m-bootloader" : "earlier open bootloader";
    bootloaderText = `${named} ${identity[1]} (${identity[0]})`;
  } else {
    bootloaderKind = "stock";
    bootloaderText = "stock (M-VAVE)";
  }

  const appStatus = appFilesStatus(flash, key);
  let firmwareText = "none";
  let modelName = null;
  if (appStatus !== AppStatus.NOT_FOUND) {
    firmwareText = "unknown";
    const app = table.find((e) => e.name === "app.bin");
    if (app && BOOT_AREA + app.addr + app.len <= flash.length) {
      const text = appRead(flash, BOOT_AREA, app.addr, app.len, key);
      const idents = findIdentities(text).sort((a, b) => (a[1] < b[1] ? -1 : a[1] > b[1] ? 1 : 0));
      const unique = [...new Map(idents.map((i) => [i.join("_"), i])).values()];
      const last = unique[unique.length - 1];
      if (last) modelName = last[0];
      const baudGirl = table.some((e) => e.name === "VM" && e.addr === 0xd9000);
      const sloop = slopVersion(text);
      if (sloop) firmwareText = `SLOOP ${sloop}${modelName?.startsWith("SMK") ? " (Snoop37)" : ""}`;
      else if (findAscii(text, "Felucca") >= 0) firmwareText = "Felucca";
      else if (last) {
        firmwareText = `${last[0]} ${last[1]}`;
        if (last[0] === "SMK-37+VA" || baudGirl) firmwareText += " (Baud Girl)";
        else if (last[1] !== "900" && last[1] !== "000" && unique.length === 1) firmwareText += " (stock)";
      }
    }
  }
  const modelText = board ? (certain ? board.name : `${board.name} (probably)`) : "unknown";
  return { evidence, board, certain, modelText, bootloaderKind, bootloaderText, appStatus, appStatusText: appStatusText(appStatus), firmwareText, modelName };
}

const refuse = (message) => {
  const e = new Error(message);
  e.refused = true;
  return e;
};

const inAny = (ranges, a) => ranges.some(([s, e]) => a < e && a + SECTOR > s);

/** The change: sectors that differ, inside what MIDI mode can write, or a refusal. */
function makePlan(title, current, target, board, notes, warnings) {
  const sectors = [];
  for (let a = 0; a < current.length; a += SECTOR) {
    let same = true;
    for (let i = 0; i < SECTOR; i++) if (current[a + i] !== target[a + i]) { same = false; break; }
    if (same) continue;
    if (a < BOOT_AREA || a >= 0xff000 || inAny(board.keep, a)) throw refuse(`internal error: the change at ${hex(a, 5)} can't be written over MIDI`);
    sectors.push(a);
  }
  const runs = [];
  for (const a of sectors) {
    const last = runs[runs.length - 1];
    if (last && last[1] === a) last[1] = a + SECTOR;
    else runs.push([a, a + SECTOR]);
  }
  return { title, target, notes, warnings, sectors, runs, size: sectors.length ? sectors[sectors.length - 1] + SECTOR : 0 };
}

export function describePlan(plan) {
  const lines = [plan.title];
  if (!plan.sectors.length) lines.push("Nothing to write: the flash already holds this.");
  else lines.push(`Writes ${plan.sectors.length} sectors (${plan.sectors.length * 4} KiB): ${rangesText(plan.runs)}`);
  return lines.concat(plan.notes).join("\n");
}

const equal = (a, b) => a.length === b.length && a.every((v, i) => v === b[i]);

/** Restoring a backup: everything MIDI mode can reach. */
export function planRestore(session, data, backupUnit) {
  if (data.length !== FLASH_SIZE) throw refuse(`not a full backup: ${data.length} bytes, ${FLASH_SIZE} expected`);
  const { flash, analysis, unitId, key } = session;
  const board = analysis.board;
  if (!board) throw refuse("the model of this unit is not certain, so nothing can be written from here");
  const backup = analyse(data, null, CHIP_KEY);
  if (backup.board && backup.board.key !== board.key) throw refuse(`this backup is from a ${backup.board.name}, the connected unit is a ${board.name}`);
  if (appFilesStatus(data, key) === AppStatus.NOT_FOUND) throw refuse("this backup holds no firmware for this unit's chip key, so MIDI mode would refuse it");
  const notes = [];
  const warnings = [];
  const target = flash.slice();
  target.set(data.subarray(BOOT_AREA, 0xff000), BOOT_AREA);
  for (const [a, b] of board.keep) target.set(flash.subarray(a, b), a);
  notes.push("Restores firmware, settings and presets. The bootloader and the key area stay as they are.");
  if (!equal(data.subarray(0, BOOT_AREA), flash.subarray(0, BOOT_AREA))) {
    warnings.push("The backup's bootloader differs from this unit's. It isn't restored from here: use M-Flasher on a computer for that.");
  }
  for (const [a, b] of board.keep) {
    if (!equal(data.subarray(a, b), flash.subarray(a, b))) warnings.push(`This unit's Bluetooth pairing and calibration at ${rangesText([[a, b]])} differ from the backup's, and stay as they are.`);
  }
  if (backupUnit && unitId && backupUnit !== unitId) warnings.push("This backup is from another unit. Its factory data isn't restored: this unit keeps its own.");
  return makePlan("Restore a backup", flash, target, board, notes, warnings);
}

/** Installing firmware from a package, as M-Flasher's window does. */
export function planFirmware(session, raw, wipeSettings) {
  const { flash, analysis, key } = session;
  const board = analysis.board;
  if (!board) throw refuse("the model of this unit is not certain, so nothing can be written from here");
  const pkg = parsePackage(raw);
  if (!board.models.includes(pkg.model)) throw refuse(`this package is for the ${pkg.model}, the connected unit is a ${board.name}`);
  const pkgFlash = packageFlash(pkg);
  if (pkgFlash.length > FLASH_SIZE) throw refuse("the package is larger than the flash");
  const padded = new Uint8Array(FLASH_SIZE).fill(0xff);
  padded.set(pkgFlash);
  if (appFilesStatus(padded, key) !== AppStatus.OK) throw refuse(`the package's firmware does not decrypt with this unit's chip key (${hex(key, 4)}): writing it would not start`);
  const notes = [];
  const warnings = [];
  if (analysis.modelName && analysis.modelName !== "SMK-37+VA" && analysis.modelName !== pkg.model) {
    warnings.push(`The unit runs ${analysis.modelName} firmware; this package is for the ${pkg.model}. The unit will then report itself as that model, and M-UPGRADE will only take that model's packages.`);
  }
  const regions = (img) => appTable(img, key).filter((e) => e.attr === 0x12 || e.attr === 0x92).map((e) => `${e.name}:${e.addr}:${e.len}`).sort().join();
  const old = regions(flash);
  if (old && old !== regions(padded)) {
    warnings.push(`The installed firmware (${analysis.firmwareText}) keeps its data in other places than ${pkg.identity}; what it leaves behind can confuse the new firmware (wrong presets, no sound). To go back to a firmware cleanly, restore a backup made while it was installed.`);
  }
  const target = flash.slice();
  target.set(pkgFlash.subarray(BOOT_AREA), BOOT_AREA);
  for (const [a, b] of board.keep) target.set(flash.subarray(a, b), a);
  notes.push(`Installs ${pkg.identity} (${rangesText([[BOOT_AREA, pkgFlash.length]])}). The bootloader stays as it is.`);
  if (wipeSettings) {
    for (const e of appTable(padded, key)) {
      if ((e.name !== "VM" && e.name !== "BTIF") || (e.attr !== 0x12 && e.attr !== 0x92) || !e.len || e.addr % SECTOR || e.addr + e.len > FLASH_SIZE) continue;
      if (board.factory.some(([a, b]) => a < e.addr + e.len && e.addr < b)) throw refuse(`the package declares ${e.name} over factory data`);
      if (inAny(board.keep, e.addr)) {
        notes.push(`${e.name} is left as it is: MIDI mode keeps it.`);
        continue;
      }
      target.fill(0xff, e.addr, e.addr + e.len);
      notes.push(`Erases ${e.name} (${rangesText([[e.addr, e.addr + e.len]])}): ${e.name === "VM" ? "settings" : "Bluetooth pairings"}`);
    }
  }
  return makePlan(`Install firmware ${pkg.identity}`, flash, target, board, notes, warnings);
}
