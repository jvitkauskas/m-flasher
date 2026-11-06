// SPDX-License-Identifier: MIT
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { FINISH, Link, Unit, findPorts, pack7, parseMessage, unpack7 } from "../lib/protocol.js";

const hexBytes = (text) => Uint8Array.from(text.trim().split(/\s+/).map((x) => parseInt(x, 16)));

function frame(command, body) {
  let sum = 0;
  for (const b of body) sum += b;
  const n = body.length;
  return Uint8Array.from([0xf0, ...pack7([0, 0x59, command, n & 255, n >>> 8, 0, ...body, ~sum & 255]), 0xf7]);
}

/** A fake pair of ports: `answer` gets what the page sends, and returns what comes back. */
function ports(answer) {
  const input = { state: "connected", onmidimessage: null };
  const sent = [];
  const output = {
    state: "connected",
    send(bytes) {
      sent.push(Uint8Array.from(bytes));
      const reply = answer(Uint8Array.from(bytes));
      if (reply) queueMicrotask(() => input.onmidimessage?.({ data: reply }));
    },
  };
  return { input, output, sent };
}

test("frames match what firmware and the chip are known to take", async () => {
  const p = ports(() => null);
  const unit = new Unit(new Link(p.input, p.output));
  await unit.identity(1);
  // The identity query every installer sends.
  assert.deepEqual(p.sent[0], hexBytes("F0 00 32 45 00 00 00 40 7F F7"));
  // A 16-byte read at 0x4120, worked out independently from stock firmware's code.
  p.sent.length = 0;
  await assert.rejects(unit.read(0x4120, 16, 1));
  assert.deepEqual(p.sent[0], hexBytes("F0 00 32 0D 41 00 00 00 00 20 02 01 00 00 02 00 00 0E 01 F7"));
});

test("pack7 and unpack7 are inverses", () => {
  const data = Uint8Array.from({ length: 700 }, (_, i) => (i * 37) & 255);
  assert.deepEqual(unpack7(pack7(data)).subarray(0, 700), data);
});

test("a message with a wrong checksum or length is ignored", () => {
  const good = frame(0x23, [1, 2, 3]);
  assert.ok(parseMessage(good));
  const body = unpack7(good.subarray(1, -1));
  body[6] ^= 1;
  assert.equal(parseMessage(Uint8Array.from([0xf0, ...pack7(body), 0xf7])), null);
  const shorter = unpack7(good.subarray(1, -1)).subarray(0, 8);
  assert.equal(parseMessage(Uint8Array.from([0xf0, ...pack7(shorter), 0xf7])), null);
});

test("the page can't send the commands that erase or write on stock firmware", () => {
  const source = readFileSync(new URL("../lib/protocol.js", import.meta.url), "utf8");
  const allowed = /HOST_COMMANDS = Object\.freeze\(\{([^}]*)\}\)/.exec(source)[1];
  assert.deepEqual(allowed.match(/0x[0-9a-f]+/g), ["0x11", "0x12", "0x23", "0x24"]);
  // Package replies are the only other command, and go through frame() in serveUpdate alone.
  assert.equal(source.match(/frame\(/g).length, 3, "one definition, one in #ask, one for package replies");
});

test("a refused read throws, and a garbled or missing one retries", async () => {
  let asked = 0;
  const data = Uint8Array.from({ length: 16 }, (_, i) => i);
  const p = ports((sent) => {
    asked++;
    const m = parseMessage(sent);
    if (asked === 1) return null;
    if (asked === 2) return frame(0x23, [9, 9, 9, 9, 9, 9, 9, 9, ...data]);
    return frame(0x23, [...m.body, ...data]);
  });
  const unit = new Unit(new Link(p.input, p.output));
  assert.deepEqual(await unit.read(0x100, 16, 5), data);
  assert.equal(asked, 3);
  const refused = ports(() => frame(0, [1]));
  await assert.rejects(new Unit(new Link(refused.input, refused.output)).read(0, 16, 50), /refused/);
});

test("info tells MIDI mode and the stage apart, and firmware from either", async () => {
  const body = [...new TextEncoder().encode("MBOOT"), 1, 2, 0, 0x0f, 0x98, 0, 0, 0x10, 0, 0x14, 0x60, 0x85, 0, 1, 2, 3, 4, 5, 6, 7, 8, 0, 0, ...new TextEncoder().encode("2.2"), 0];
  const p = ports(() => frame(0x12, body));
  const info = await new Unit(new Link(p.input, p.output)).info(50);
  assert.deepEqual(info, { protocol: 1, kind: "stage", key: 0x980f, capacity: 0x100000, jedec: 0x856014, unitId: "0102030405060708", version: "2.2" });
  const firmware = ports(() => frame(0x12, [0, 0, 0, 0, 0, 0]));
  assert.equal(await new Unit(new Link(firmware.input, firmware.output)).info(50), null);
});

test("an update serves the package, and stops at the finishing address", async () => {
  const image = Uint8Array.from({ length: 2000 }, (_, i) => i & 255);
  const request = (address, count) => frame(0x30, [0, address & 255, (address >>> 8) & 255, (address >>> 16) & 255, (address >>> 24) & 255, count & 255, count >>> 8, 0]);
  const script = [request(0, 512), request(512, 512), request(FINISH.LOADER, 8)];
  const seen = [];
  const p = ports((sent) => {
    if (sent[1] === 0x22) return script.shift();
    const m = parseMessage(sent);
    assert.equal(m.command, 0x30);
    seen.push(m.body);
    return script.shift();
  });
  await new Unit(new Link(p.input, p.output)).serveUpdate(image, { finish: FINISH.LOADER, idleMs: 200 });
  assert.deepEqual(seen[0].subarray(8), image.subarray(0, 512));
  assert.deepEqual(seen[1].subarray(8), image.subarray(512, 1024));
  assert.equal(new TextDecoder().decode(seen[2].subarray(8, 15)), "success");
  assert.deepEqual(p.sent[0], hexBytes("F0 22 24 35 7F F7"));
});

test("an update refuses to read outside the package", async () => {
  const p = ports((sent) => (sent[1] === 0x22 ? frame(0x30, [0, 0, 0, 1, 0, 0, 2, 0]) : null));
  await assert.rejects(new Unit(new Link(p.input, p.output)).serveUpdate(new Uint8Array(100), { finish: FINISH.LOADER, idleMs: 200 }), /outside the package/);
});

test("findPorts takes units only, the first port first", () => {
  const port = (name) => ({ name, state: "connected" });
  const map = (names) => new Map(names.map((n, i) => [i, port(n)]));
  const access = {
    outputs: map(["Some Synth", "MIDIOUT2 (SMK-37 Pro)", "SMK-37 Pro"]),
    inputs: map(["Some Synth", "MIDIOUT2 (SMK-37 Pro)", "SMK-37 Pro"]),
  };
  assert.deepEqual(findPorts(access).map((p) => p.name), ["SMK-37 Pro", "MIDIOUT2 (SMK-37 Pro)"]);
});
