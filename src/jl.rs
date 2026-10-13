//! JieLi UBOOT1.00 (mask ROM USB download mode) and M-Flasher's flash loader.
//!
//! The ROM accepts memory writes and a jump. The tool writes the loader into
//! RAM and starts it; the loader then answers the flash commands, plus an
//! on-chip CRC32, the flash's unique ID and the unit's display. The loader
//! (data/mflr-loader-a/b.bin) is built by m-bootloader
//! (https://github.com/jvitkauskas/m-bootloader, MIT).
//! Commands are 16-byte CDBs: a 16-bit opcode, big-endian arguments, padded
//! with 0xFF. Protocol after jl-uboot-tool (MIT, (c) 2023 Andrey Grigoryev).

use crate::formats::{crc16, crc16_update};
use crate::scsi::{Data, PRODUCT, Scsi, ScsiError, VENDOR};

/// Ours, plain (scrambled for the ROM when uploaded), built for two addresses:
/// a new one never overwrites one that is running.
const OWN_A: (u32, &[u8]) = (0x01C2_0000, include_bytes!("../data/mflr-loader-a.bin"));
const OWN_B: (u32, &[u8]) = (0x01C2_8000, include_bytes!("../data/mflr-loader-b.bin"));
const IDENTIFY: u16 = 0xFC40;
const CRC32: u16 = 0xFC41;
const UNIQUE_ID: u16 = 0xFC42;
const DISPLAY_ON: u16 = 0xFC43;
const DISPLAY_TEXT: u16 = 0xFC44;
const DISPLAY_BAR: u16 = 0xFC45;
const LOADER_ARG: u16 = 1; // target: SPI NOR flash

const WRITE_MEMORY: u16 = 0xFB06;
const JUMP: u16 = 0xFB08;
const ERASE_BLOCK: u16 = 0xFB00;
const ERASE_SECTOR: u16 = 0xFB01;
const WRITE_FLASH: u16 = 0xFB04;
const READ_FLASH: u16 = 0xFD05;
const READ_KEY: u16 = 0xFC09;
const ONLINE_DEVICE: u16 = 0xFC0A;
#[allow(dead_code)] // the loader's on-chip CRC: for a faster verify later
const FLASH_CRC16: u16 = 0xFC0E;
const USB_BUFFER_SIZE: u16 = 0xFC14;

/// JieLi's "CrcDecode" cipher (the chip key comes back in it).
pub fn crc_cipher(data: &[u8], key: u32) -> Vec<u8> {
    const MAGIC: &[u8] = &[0xC3, 0xCF, 0xC0, 0xE8, 0xCE, 0xD2, 0xB0, 0xAE, 0xC4, 0xE3, 0xA3, 0xAC, 0xD3, 0xF1, 0xC1, 0xD6];
    let mut c = crc16_update((key & 0xFFFF) as u16, &((key >> 16) as u16).to_le_bytes());
    data.iter()
        .enumerate()
        .map(|(i, &b)| {
            c = crc16_update(c, &MAGIC[i % MAGIC.len()..i % MAGIC.len() + 1]);
            b ^ c as u8
        })
        .collect()
}

fn cdb(cmd: u16, args: &[u8]) -> [u8; 16] {
    let mut c = [0xFFu8; 16];
    c[..2].copy_from_slice(&cmd.to_be_bytes());
    c[2..2 + args.len()].copy_from_slice(args);
    c
}

fn addr_len_crc(addr: u32, data: &[u8]) -> Vec<u8> {
    let mut a = addr.to_be_bytes().to_vec();
    a.extend((data.len() as u16).to_be_bytes());
    a.push(0);
    a.extend(crc16(data).to_le_bytes());
    a
}

/// A line or bar on the unit's own display.
pub enum Show<'a> {
    /// Board: 1 FM-1, 2 SMK-37.
    On(u8),
    Text(u8, &'a str, u16),
    Bar(u8, u16),
}

/// What the operations need from a unit: the identity and flash access, and
/// the loader's extras (the defaults, none, are for test doubles).
pub trait Flash: Send {
    fn loader(&self) -> String {
        "unknown".into()
    }
    /// CRC32 (IEEE) of n bytes at addr, computed on the chip.
    fn crc32(&mut self, _addr: usize, _n: usize) -> Result<Option<u32>, ScsiError> {
        Ok(None)
    }
    /// The flash chip's factory-unique ID, hex.
    fn unique_id(&mut self) -> Option<String> {
        None
    }
    fn show(&mut self, _s: Show) {}
    fn path(&self) -> &str;
    fn chip_key(&self) -> u16;
    fn flash_id(&self) -> u32;
    fn read(&mut self, addr: usize, n: usize, progress: &mut dyn FnMut(usize)) -> Result<Vec<u8>, ScsiError>;
    /// Erase [addr, addr + n): 64 KiB blocks where aligned, else 4 KiB sectors.
    fn erase(&mut self, addr: usize, n: usize) -> Result<(), ScsiError>;
    /// Program erased flash; progress(bytes done).
    fn program(&mut self, addr: usize, data: &[u8], progress: &mut dyn FnMut(usize)) -> Result<(), ScsiError>;
    /// Transfers that failed and were sent again so far, and the last failure.
    fn retries(&self) -> (u32, String) {
        (0, String::new())
    }
}

/// An open download-mode session with the loader running.
pub struct Connection<S: Scsi> {
    path: String,
    dev: S,
    chip_key: u16,
    flash_id: u32,
    io: usize,
    retried: u32,
    last_retry: String,
    /// Bytes per read command.
    read_io: usize,
    /// The running loader's version.
    own: String,
    /// The CPU clock our loader runs the chip at (MHz; 0: it does not say).
    mhz: u8,
}

impl<S: Scsi> Connection<S> {
    pub fn start(path: &str, mut dev: S, log: &mut dyn FnMut(String)) -> Result<Self, ScsiError> {
        let (vendor, product) = dev.inquiry()?;
        if vendor != VENDOR || product != PRODUCT {
            return Err(ScsiError::Failed(format!("{path} is \"{vendor} {product}\", not a {VENDOR} {PRODUCT} download-mode device")));
        }
        let mut c = Connection { path: path.into(), dev, chip_key: 0, flash_id: 0, io: 512, read_io: 4096,
                                 retried: 0, last_retry: String::new(), own: String::new(), mhz: 0 };
        // What is running already? A loader stays registered until the unit is
        // switched off. (Commands the ROM alone does not know just fail.)
        let ours_running = c.identify();
        if ours_running.is_none() && c.call(ONLINE_DEVICE, &[]).is_ok() {
            // another tool's loader (JieLi's): jumping through it is not safe
            return Err(ScsiError::Failed(
                "another flash loader is running on the unit: switch it off and on in download mode, then connect again".into()));
        }
        let bundled = c.bundled_version();
        if let Some((v, _)) = ours_running.as_ref().filter(|(v, _)| *v == bundled) {
            log(format!("M-Flasher loader {v} already running"));
            c.own = v.clone();
        } else {
            // ours, at the address the running one (if any) does not use
            let (addr, image) = match &ours_running {
                None => OWN_A,
                Some((_, at)) if *at == OWN_B.0 => OWN_A,
                Some(_) => OWN_B,           // at A, or an older build that does not say where
            };
            log(format!("Starting the M-Flasher loader {bundled}"));
            let scrambled: Vec<Vec<u8>> = image.chunks(512).map(|b| crc_cipher(b, 0xFFFF_FFFF)).collect();
            c.upload_and_jump(addr, &scrambled)?;
            match c.identify() {
                Some((v, _)) => c.own = v,
                None => return Err(ScsiError::Failed(
                    "the M-Flasher loader did not start: switch the unit off and on in download mode, then connect again".into())),
            }
        }
        c.read_io = 16384;
        c.mhz = c.call(IDENTIFY, &[]).map(|r| r[13]).unwrap_or(0);
        let r = c.call(READ_KEY, &0xAC6900u32.to_be_bytes())?;
        c.chip_key = u16::from_le_bytes(crc_cipher(&[r[5], r[4]], 0xFFFF_FFFF).try_into().unwrap());
        let r = c.call(ONLINE_DEVICE, &[])?;
        c.flash_id = u32::from_le_bytes([r[2], r[3], r[4], r[5]]);
        if let Ok(r) = c.call(USB_BUFFER_SIZE, &[]) {
            c.io = (u32::from_be_bytes([r[0], r[1], r[2], r[3]]) as usize).clamp(256, 4096);
        }
        Ok(c)
    }

    /// Our loader's (version, address), if it is the one answering.
    fn identify(&mut self) -> Option<(String, u32)> {
        let r = self.call(IDENTIFY, &[]).ok()?;
        (&r[..4] == b"MFLR").then(|| {
            (String::from_utf8_lossy(&r[4..7]).trim_end_matches('\0').to_string(),
             u32::from_be_bytes([r[8], r[9], r[10], r[11]]))
        })
    }

    /// The version of the loader bundled in this build.
    fn bundled_version(&self) -> String {
        let img = OWN_A.1;
        let at = img.windows(4).position(|w| w == b"MFLR").unwrap_or(0);
        String::from_utf8_lossy(&img[at + 4..at + 7]).to_string()
    }

    fn upload_and_jump(&mut self, addr: u32, blocks: &[Vec<u8>]) -> Result<(), ScsiError> {
        for (i, block) in blocks.iter().enumerate() {
            let a = addr + (i * 512) as u32;
            self.dev.execute(&cdb(WRITE_MEMORY, &addr_len_crc(a, block)), Data::Out(block))?;
        }
        let mut args = addr.to_be_bytes().to_vec();
        args.extend(LOADER_ARG.to_be_bytes());
        self.call(JUMP, &args)?;
        std::thread::sleep(std::time::Duration::from_millis(100));
        Ok(())
    }

    /// A flash command, tried up to RETRIES times: a failed transfer (Windows
    /// reports USB hiccups as error 31) is sent again after a pause. Reading,
    /// erasing and programming the same data again are all harmless.
    fn retry(&mut self, what: &str, mut f: impl FnMut(&mut S) -> Result<(), ScsiError>) -> Result<(), ScsiError> {
        const RETRIES: u32 = 4;
        let mut attempt = 1;
        loop {
            match f(&mut self.dev) {
                Ok(()) => return Ok(()),
                Err(ScsiError::Failed(e)) if attempt < RETRIES => {
                    self.retried += 1;
                    self.last_retry = format!("{what}: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(250 * attempt as u64));
                    attempt += 1;
                }
                Err(ScsiError::Failed(e)) => {
                    return Err(ScsiError::Failed(format!("{what}: {e} (tried {RETRIES} times)")));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// A command without data; returns the 14 answer bytes.
    fn call(&mut self, cmd: u16, args: &[u8]) -> Result<[u8; 14], ScsiError> {
        let mut resp = [0u8; 16];
        let c = cdb(cmd, args);
        let what = match cmd {
            ERASE_BLOCK | ERASE_SECTOR => format!("erasing 0x{:05X}", u32::from_be_bytes([args[0], args[1], args[2], args[3]])),
            _ => format!("command 0x{cmd:04x}"),
        };
        self.retry(&what, |d| d.execute(&c, Data::In(&mut resp)).map(|_| ()))?;
        if u16::from_be_bytes([resp[0], resp[1]]) != cmd {
            return Err(ScsiError::Failed(format!("unexpected answer to command 0x{cmd:04x}")));
        }
        Ok(resp[2..].try_into().unwrap())
    }

    /// The loader's CRC16 of n plain bytes at addr (n < 64 KiB).
    #[allow(dead_code)]
    pub fn flash_crc16(&mut self, addr: usize, n: usize) -> Result<u16, ScsiError> {
        let mut a = (addr as u32).to_be_bytes().to_vec();
        a.extend((n as u16).to_be_bytes());
        let r = self.call(FLASH_CRC16, &a)?;
        Ok(u16::from_be_bytes([r[0], r[1]]))
    }
}

impl<S: Scsi> Flash for Connection<S> {
    fn loader(&self) -> String {
        match self.mhz {
            0 => format!("M-Flasher {}", self.own),
            mhz => format!("M-Flasher {}, CPU at {mhz} MHz", self.own),
        }
    }

    fn crc32(&mut self, addr: usize, n: usize) -> Result<Option<u32>, ScsiError> {
        let mut a = (addr as u32).to_be_bytes().to_vec();
        a.extend((n as u32).to_be_bytes());
        let r = self.call(CRC32, &a)?;
        Ok(Some(u32::from_be_bytes([r[0], r[1], r[2], r[3]])))
    }

    fn unique_id(&mut self) -> Option<String> {
        let r = self.call(UNIQUE_ID, &[]).ok()?;
        let id = &r[..8];
        (id.iter().any(|&b| b != 0xFF) && id.iter().any(|&b| b != 0)).then(|| id.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Best effort: a display problem must never stop a write.
    fn show(&mut self, s: Show) {
        let _ = match s {
            Show::On(board) => self.call(DISPLAY_ON, &[board]).map(|_| ()),
            Show::Bar(pct, color) => {
                let mut a = vec![pct];
                a.extend(color.to_be_bytes());
                self.call(DISPLAY_BAR, &a).map(|_| ())
            }
            Show::Text(row, text, fg) => {
                let t: Vec<u8> = text.bytes().take(20).collect();
                let mut a = vec![row];
                a.extend(fg.to_be_bytes());
                a.extend(0u16.to_be_bytes());
                a.extend((t.len() as u16).to_be_bytes());
                self.dev.execute(&cdb(DISPLAY_TEXT, &a), Data::Out(&t)).map(|_| ())
            }
        };
    }

    fn retries(&self) -> (u32, String) {
        (self.retried, self.last_retry.clone())
    }
    fn path(&self) -> &str {
        &self.path
    }
    fn chip_key(&self) -> u16 {
        self.chip_key
    }
    fn flash_id(&self) -> u32 {
        self.flash_id
    }

    fn read(&mut self, addr: usize, n: usize, progress: &mut dyn FnMut(usize)) -> Result<Vec<u8>, ScsiError> {
        let mut out = vec![0u8; n];
        let mut done = 0;
        while done < n {
            let k = self.read_io.min(n - done);
            let mut args = ((addr + done) as u32).to_be_bytes().to_vec();
            args.extend((k as u16).to_be_bytes());
            let c = cdb(READ_FLASH, &args);
            let buf = &mut out[done..done + k];
            // a short read would leave the rest of the chunk unread
            self.retry(&format!("reading 0x{:05X}", addr + done), |d| match d.execute(&c, Data::In(buf))? {
                got if got == k => Ok(()),
                got => Err(ScsiError::Failed(format!("{got} of {k} bytes received"))),
            })?;
            done += k;
            progress(done);
        }
        Ok(out)
    }

    fn erase(&mut self, addr: usize, n: usize) -> Result<(), ScsiError> {
        let (mut a, end) = (addr, addr + n);
        while a < end {
            if a % 0x10000 == 0 && end - a >= 0x10000 {
                self.call(ERASE_BLOCK, &(a as u32).to_be_bytes())?;
                a += 0x10000;
            } else {
                self.call(ERASE_SECTOR, &(a as u32).to_be_bytes())?;
                a += 0x1000;
            }
        }
        Ok(())
    }

    fn program(&mut self, addr: usize, data: &[u8], progress: &mut dyn FnMut(usize)) -> Result<(), ScsiError> {
        for (i, chunk) in data.chunks(self.io).enumerate() {
            if chunk.iter().any(|&b| b != 0xFF) {
                let a = (addr + i * self.io) as u32;
                let c = cdb(WRITE_FLASH, &addr_len_crc(a, chunk));
                self.retry(&format!("writing 0x{a:05X}"), |d| d.execute(&c, Data::Out(chunk)).map(|_| ()))?;
            }
            progress(i * self.io + chunk.len());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_cipher_magic_is_the_gb2312_phrase() {
        // jl-uboot-tool: "孟黎我爱你，玉林" in GB2312
        assert_eq!(crc_cipher(&[0, 0], 0xFFFF_FFFF).len(), 2);
    }
}
