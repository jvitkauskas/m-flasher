// SPDX-License-Identifier: MIT
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";
import { createHash } from "node:crypto";
import * as f from "../lib/formats.js";
import { hasOwnUpdater, variantOf } from "../lib/assets.js";
import { CHIP_KEY, FM1, SMK37, analyse, flashText, planFirmware, planRestore } from "../lib/unit.js";

const DATA = process.env.FLASHER_TEST_DATA ?? "";
const need = (name) => {
  const p = `${DATA}/${name}`;
  return DATA && existsSync(p) ? new Uint8Array(readFileSync(p)) : null;
};

test("crc16, crc32 and enc match their check values", () => {
  const text = new TextEncoder().encode("123456789");
  assert.equal(f.crc16(text), 0x31c3);
  assert.equal(f.crc32(text), 0xcbf43926);
  assert.deepEqual(f.enc(f.enc(text, 0xffff), 0xffff), text);
});

test("a package from an image reads back as what MIDI mode expects", () => {
  const flash = new Uint8Array(0x100000).fill(0x5a);
  const image = f.packageImage(flash, 0x5000);
  const head = f.enc(image.subarray(0, 0x40), 0xffff);
  assert.equal(f.crc16(head.subarray(2)), head[0] | (head[1] << 8));
  assert.equal(head[8] | (head[9] << 8), 1);
  assert.equal(f.crc16(image.subarray(0x40, 0x90)), head[2] | (head[3] << 8));
  const entry = f.enc(image.subarray(0x40, 0x90), 0xffff);
  assert.equal(entry[0] | (entry[1] << 8), 0);
  assert.equal(entry[8] | (entry[9] << 8), 0x400);
  assert.equal(entry[12] | (entry[13] << 8), 0x5000);
  assert.equal(entry[4] | (entry[5] << 8), f.crc16(flash.subarray(0, 0x5000)));
  assert.equal(image.length, 0x5400);
});

test("real packages parse", { skip: !need("fm1-015.fwsc") && "no test data" }, () => {
  for (const [name, identity] of [["fm1-015.fwsc", "FM-1_015"], ["smk37-016.fwsc", "SMK-37 Pro_016"], ["smk37elite-016.fwsc", "SMK-37 Elite_016"]]) {
    const pkg = f.parsePackage(need(name));
    assert.equal(pkg.identity, identity);
    const flash = f.packageFlash(pkg);
    const padded = new Uint8Array(f.FLASH_SIZE).fill(0xff);
    padded.set(flash);
    assert.equal(f.appFilesStatus(padded, CHIP_KEY), f.AppStatus.OK);
  }
});

// Against what M-Flasher itself said about the same dumps, kept beside them as .json.
const dumps = process.env.FLASHER_DUMPS ? JSON.parse(readFileSync(process.env.FLASHER_DUMPS, "utf8")) : [];
for (const d of dumps) {
  test(`analysis agrees with M-Flasher on ${d.file}`, () => {
    const flash = new Uint8Array(readFileSync(d.file));
    assert.equal(createHash("sha256").update(flash).digest("hex"), d.sha256);
    const a = analyse(flash, d.flash, CHIP_KEY);
    assert.equal(a.modelText, d.model);
    assert.equal(a.bootloaderText, d.bootloader);
    assert.equal(a.firmwareText, d.firmware);
    assert.equal(a.appStatusText, d.app);
  });
}

const session = (flash, board) => ({ flash, key: CHIP_KEY, unitId: "u1", analysis: { board, modelName: board.models[0], firmwareText: "x" } });

test("a restore can't reach the boot area, key area, or the SMK-37's calibration", { skip: !need("smk37-016.fwsc") && "no test data" }, () => {
  const flash = new Uint8Array(f.FLASH_SIZE).fill(0xff);
  flash.set(f.packageFlash(f.parsePackage(need("smk37-016.fwsc"))));
  const data = flash.slice();
  data.fill(0x11, 0, 0x1000);
  data.fill(0x22, 0xc1000, 0xc2000);
  data.fill(0x33, 0xff000);
  data.fill(0x44, 0xe0000, 0xe1000);
  const plan = planRestore(session(flash, SMK37), data, "u1");
  assert.deepEqual(plan.sectors, [0xe0000]);
  assert.equal(plan.target[0], flash[0]);
  assert.equal(plan.target[0xc1000], flash[0xc1000]);
  assert.equal(plan.target[0xff000], flash[0xff000]);
  assert.ok(plan.warnings.some((w) => w.includes("bootloader")));
  assert.ok(plan.warnings.some((w) => w.includes("calibration")));
});

test("a firmware of another model or chip key is refused", { skip: !need("smk37-016.fwsc") && "no test data" }, () => {
  const flash = new Uint8Array(f.FLASH_SIZE).fill(0xff);
  assert.throws(() => planFirmware(session(flash, FM1), need("smk37-016.fwsc")), /for the SMK-37 Pro/);
  const other = session(flash, SMK37);
  other.key = 0x1234;
  assert.throws(() => planFirmware(other, need("smk37-016.fwsc")), /chip key/);
  const plan = planFirmware(session(flash, SMK37), need("smk37-016.fwsc"));
  assert.ok(plan.sectors.length > 0 && plan.sectors[0] >= 0x4000);
});

test("installing firmware writes only the package", { skip: !need("fm1-015.fwsc") && "no test data" }, () => {
  const flash = new Uint8Array(f.FLASH_SIZE).fill(0x77);
  const plan = planFirmware(session(flash, FM1), need("fm1-015.fwsc"));
  assert.ok(plan.target.subarray(0x93000).every((b) => b === 0x77), "settings, pairings and the rest are kept");
  assert.ok(plan.target.subarray(0, f.BOOT_AREA).every((b) => b === 0x77), "the bootloader stays");
  assert.ok(plan.sectors.every((a) => a >= f.BOOT_AREA && a < 0x93000));
});

test("only firmware with its own updater is kept from the update route", () => {
  for (const id of ["SMK-37_930", "ota-SMK-37_930", "SMK-37_901"]) assert.ok(hasOwnUpdater(id), id);
  for (const id of ["SMK-37 Pro_016", "SMK-37 Pro_900", "SMK-37 Elite_920", "SMK-37+VA_102", "FM-1_918", "ota-SMK-37 Pro_900", null]) assert.ok(!hasOwnUpdater(id), id);
  assert.equal(variantOf("SMK-37"), null);
});

test("the FM-1's two flash chips are told apart", () => {
  assert.equal(flashText(0x856014), "0x856014 (old)");
  assert.equal(flashText(0xb36014), "0xB36014 (new)");
  assert.equal(flashText(0xeb6014), "0xEB6014", "the SMK-37 has one");
  assert.equal(flashText(0xef4014), "0xEF4014");
});
