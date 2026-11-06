// SPDX-License-Identifier: MIT
// Talking to a unit over USB MIDI: SysEx frames, what it can be asked, and serving it a package.
//
// One frame family is shared with firmware's update service, whose other commands erase and
// write flash without any check, and whose checksum doesn't cover the command byte. So this file
// can only ever build the commands in ALLOWED: nothing here can send them another way.
const HOST_COMMANDS = Object.freeze({ IDENTITY: 0x11, INFO: 0x12, READ: 0x23, CHECK: 0x24 });
const ALLOWED = new Set(Object.values(HOST_COMMANDS));
const PACKAGE_REPLY = 0x30;
const SOFT_UPGRADE = Uint8Array.of(0xf0, 0x22, 0x24, 0x35, 0x7f, 0xf7);
const SOFT_DOWNLOAD = Uint8Array.of(0xf0, 0x22, 0x24, 0x35, 0x7d, 0xf7);
const SOFT_RESTART = Uint8Array.of(0xf0, 0x22, 0x24, 0x35, 0x7e, 0xf7);
const FINISH_APP = 0xe0000000;
const FINISH_LOADER = 0xf0000000;
export const READ_CHUNK = 512;

export function pack7(data) {
  const out = [];
  let acc = 0;
  let bits = 0;
  for (const b of data) {
    acc |= b << bits;
    bits += 8;
    while (bits >= 7) {
      out.push(acc & 0x7f);
      acc >>>= 7;
      bits -= 7;
    }
  }
  if (bits) out.push(acc & 0x7f);
  return out;
}

export function unpack7(data) {
  const out = [];
  let acc = 0;
  let bits = 0;
  for (const b of data) {
    acc |= (b & 0x7f) << bits;
    bits += 7;
    while (bits >= 8) {
      out.push(acc & 0xff);
      acc >>>= 8;
      bits -= 8;
    }
  }
  return Uint8Array.from(out);
}

function frame(command, body) {
  const n = body.length;
  let sum = 0;
  for (const b of body) sum += b;
  const decoded = new Uint8Array(7 + n);
  decoded.set([0, 0x59, command, n & 0xff, (n >>> 8) & 0xff, (n >>> 16) & 0xff]);
  decoded.set(body, 6);
  decoded[6 + n] = ~sum & 0xff;
  return Uint8Array.from([0xf0, ...pack7(decoded), 0xf7]);
}

/** A decoded message, or null if it isn't one, or fails its length or checksum. */
export function parseMessage(sysex) {
  if (sysex[0] !== 0xf0 || sysex[sysex.length - 1] !== 0xf7) return null;
  const d = unpack7(sysex.subarray(1, -1));
  if (d.length < 7 || d[0] !== 0 || d[1] !== 0x59) return null;
  const n = d[3] | (d[4] << 8) | (d[5] << 16);
  if (d.length !== n + 7) return null;
  let sum = 0;
  for (let i = 6; i < 6 + n; i++) sum += d[i];
  if ((~sum & 0xff) !== d[6 + n]) return null;
  return { command: d[2], body: d.subarray(6, 6 + n) };
}

const metadata = (address, count) => Uint8Array.of(
  0, address & 0xff, (address >>> 8) & 0xff, (address >>> 16) & 0xff, (address >>> 24) & 0xff,
  count & 0xff, (count >>> 8) & 0xff, (count >>> 16) & 0xff,
);

const u32 = (b, at) => (b[at] | (b[at + 1] << 8) | (b[at + 2] << 16) | (b[at + 3] << 24)) >>> 0;

const sameBytes = (a, b) => a.length === b.length && a.every((v, i) => v === b[i]);

export class UnitLost extends Error {}

/** One input and output port pair, with incoming SysEx queued. */
export class Link {
  constructor(input, output) {
    this.input = input;
    this.output = output;
    this.queue = [];
    this.waiter = null;
    this.partial = null;
    input.onmidimessage = (event) => this.received(event.data);
  }

  received(data) {
    if (data[0] === 0xf0) this.partial = [];
    else if (data[0] > 0x7f && data[0] < 0xf8) { this.partial = null; return; }
    if (!this.partial) return;
    for (const b of data) this.partial.push(b);
    if (data[data.length - 1] === 0xf7) {
      this.queue.push(Uint8Array.from(this.partial));
      this.partial = null;
      if (this.waiter) this.waiter();
    }
  }

  get lost() {
    return this.input.state === "disconnected" || this.output.state === "disconnected";
  }

  send(bytes) {
    try {
      this.output.send(bytes);
    } catch (cause) {
      throw new UnitLost(`the unit went away: ${cause.message}`);
    }
  }

  /** The next SysEx message, or null after ms. */
  async next(ms) {
    if (!this.queue.length) {
      await new Promise((resolve) => {
        const timer = setTimeout(resolve, ms);
        this.waiter = () => {
          clearTimeout(timer);
          resolve();
        };
      });
      this.waiter = null;
    }
    return this.queue.shift() ?? null;
  }

  /** Stops listening. The ports stay open: browsers don't reliably reopen a closed one at once. */
  close() {
    this.input.onmidimessage = null;
    this.waiter?.();
  }
}

/** A unit at the other end of a link. */
export class Unit {
  constructor(link) {
    this.link = link;
  }

  async #ask(command, body, ms, wanted = [command]) {
    if (!ALLOWED.has(command)) throw new Error(`refusing to send command ${command}`);
    const link = this.link;
    link.queue.length = 0;
    link.send(frame(command, body));
    const end = Date.now() + ms;
    for (; ;) {
      const left = end - Date.now();
      if (left <= 0) return null;
      const raw = await link.next(left);
      if (!raw) return null;
      const m = parseMessage(raw);
      if (m && (wanted.includes(m.command) || m.command === 0)) return m;
    }
  }

  /** The identity: "FM-1_016", or "ota-FM-1_900" from an update loader or MIDI mode. */
  async identity(ms = 1500) {
    for (let tries = 0; tries < 3; tries++) {
      const m = await this.#ask(HOST_COMMANDS.IDENTITY, new Uint8Array(0), ms);
      if (m?.command === HOST_COMMANDS.IDENTITY && m.body.length >= 1) {
        let text = "";
        for (const b of m.body) {
          if (!b) break;
          text += String.fromCharCode(b);
        }
        return text;
      }
    }
    return null;
  }

  /** What's answering, if it's MIDI mode with host reads or our RAM stage; null for anything else. */
  async info(ms = 1000) {
    const m = await this.#ask(HOST_COMMANDS.INFO, new Uint8Array(0), ms);
    if (m?.command !== HOST_COMMANDS.INFO || m.body.length < 29) return null;
    const b = m.body;
    if (String.fromCharCode(...b.subarray(0, 5)) !== "MBOOT") return null;
    let version = "";
    for (let i = 28; i < b.length && b[i]; i++) version += String.fromCharCode(b[i]);
    return {
      protocol: b[5],
      kind: b[6] === 2 ? "stage" : "bootloader",
      key: b[8] | (b[9] << 8),
      capacity: u32(b, 10),
      jedec: u32(b, 14),
      unitId: [...b.subarray(18, 26)].map((x) => x.toString(16).padStart(2, "0")).join(""),
      version,
    };
  }

  async read(address, count, ms = 1500) {
    const m = metadata(address, count);
    for (let tries = 0; tries < 4; tries++) {
      const reply = await this.#ask(HOST_COMMANDS.READ, m, ms);
      if (!reply) {
        if (this.link.lost) throw new UnitLost("the unit went away while reading");
        continue;
      }
      if (reply.command !== HOST_COMMANDS.READ) throw new Error(`the unit refused a read at 0x${address.toString(16)}`);
      if (reply.body.length === 8 + count && sameBytes(reply.body.subarray(0, 8), m)) return reply.body.subarray(8);
    }
    throw new UnitLost(`no answer to a read at 0x${address.toString(16)}`);
  }

  /** The CRC-32 the unit itself computes of a range. */
  async check(address, count, ms = 15000) {
    const m = metadata(address, count);
    const reply = await this.#ask(HOST_COMMANDS.CHECK, m, ms);
    if (!reply) throw new UnitLost("no answer to the flash check");
    if (reply.command !== HOST_COMMANDS.CHECK || reply.body.length !== 12 || !sameBytes(reply.body.subarray(0, 8), m)) throw new Error("the unit refused the flash check");
    return u32(reply.body, 8);
  }

  /** count bytes of flash from 0. */
  async readAll(count, onProgress, shouldStop) {
    const out = new Uint8Array(count);
    for (let at = 0; at < count; at += READ_CHUNK) {
      if (shouldStop?.()) throw new Error("stopped");
      out.set(await this.read(at, Math.min(READ_CHUNK, count - at)), at);
      onProgress?.(Math.min(at + READ_CHUNK, count), count);
    }
    return out;
  }

  /** Asks for ROM download mode: the native M-Flasher's way in. */
  downloadMode() {
    this.link.send(SOFT_DOWNLOAD);
  }

  /** Only MIDI mode and the stage know this: leave them for the firmware. */
  restart() {
    this.link.send(SOFT_RESTART);
  }

  /**
   * Starts an update and serves `image` until the unit asks for the finishing address. A firmware
   * finishes with E0000000 and restarts into what it stored; MIDI mode and the stage, with F0000000.
   */
  async serveUpdate(image, { finish, onProgress, idleMs = 30000, shouldStop } = {}) {
    const link = this.link;
    link.queue.length = 0;
    link.send(SOFT_UPGRADE);
    let last = Date.now();
    for (; ;) {
      if (shouldStop?.()) throw new Error("stopped");
      const raw = await link.next(1000);
      if (!raw) {
        if (link.lost) throw new UnitLost("the unit went away during the update");
        if (Date.now() - last > idleMs) throw new UnitLost(`the unit stopped asking for data for ${idleMs / 1000} s`);
        continue;
      }
      const m = parseMessage(raw);
      if (!m || m.command !== PACKAGE_REPLY || m.body.length !== 8) continue;
      last = Date.now();
      const address = u32(m.body, 1);
      const count = m.body[5] | (m.body[6] << 8) | (m.body[7] << 16);
      let data;
      if (address === FINISH_APP || address === FINISH_LOADER) {
        data = new TextEncoder().encode("success\0");
      } else {
        if (count > READ_CHUNK || address + count > image.length) throw new Error(`the unit asked for data outside the package (0x${address.toString(16)})`);
        data = image.subarray(address, address + count);
      }
      const body = new Uint8Array(8 + data.length);
      body[0] = m.body[0];
      body.set(metadata(address, data.length).subarray(1), 1);
      body.set(data, 8);
      link.send(frame(PACKAGE_REPLY, body));
      if (address === finish) return;
      if (address < 0x80000000) onProgress?.(address + count);
    }
  }
}

export const FINISH = Object.freeze({ APP: FINISH_APP, LOADER: FINISH_LOADER });

const UNIT_NAMES = ["fm-1", "fm1", "minifm", "smk-37", "smk37", "felucca", "sloop", "snoop", "melodee", "choralroot", "baud"];

/** Ports that look like a unit, each with both directions: the first port of a multi-port unit first. */
export function findPorts(access) {
  const outputs = [...access.outputs.values()].filter((p) => p.state !== "disconnected");
  const inputs = [...access.inputs.values()].filter((p) => p.state !== "disconnected");
  const rank = (name) => (/private/i.test(name) ? 0 : /^midiout/i.test(name) ? 2 : 1);
  const found = [];
  for (const output of outputs) {
    const name = output.name ?? "";
    if (!UNIT_NAMES.some((known) => name.toLowerCase().includes(known))) continue;
    const input = inputs.find((p) => p.name === name) ?? inputs.find((p) => (p.name ?? "").startsWith(name.slice(0, 4)));
    if (input) found.push({ name, input, output });
  }
  return found.sort((a, b) => rank(a.name) - rank(b.name));
}
