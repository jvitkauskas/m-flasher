//! Download mode over USB MIDI: the soft key.
//!
//! The firmwares answer the SysEx `F0 22 24 35 7D F7` on their first MIDI port
//! by restarting into the chip's USB download mode: M-VAVE's stock firmware on
//! the FM-1 (015), the SMK-37 Pro (016) and the SMK-37 Elite (016), and
//! Felucca. So a unit that is running normally, even with the stock bootloader,
//! can be reached without holding any keys.
//!
//! Linux writes to the ALSA raw-MIDI device (`/dev/snd/midiC<card>D0`, port
//! 0), with libc only, so that the program needs no ALSA library. Windows and
//! macOS go through midir (WinMM, CoreMIDI).

pub const SOFT_KEY: [u8; 6] = [0xF0, 0x22, 0x24, 0x35, 0x7D, 0xF7];

/// Names under which the units appear on USB MIDI. Nothing else is ever sent
/// the soft key.
const NAMES: [&str; 3] = ["FM-1", "SMK-37", "Felucca"];

/// A unit found on USB MIDI.
pub struct MidiUnit {
    /// As the system names it: "SMK-37 Elite", "FM-1", ...
    pub name: String,
    port: sys::Port,
}

fn known(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    NAMES.iter().any(|k| n.contains(&k.to_ascii_lowercase()))
}

/// The units on USB MIDI, each by its first port.
pub fn find_units() -> Vec<MidiUnit> {
    sys::units().into_iter().map(|(name, port)| MidiUnit { name, port }).collect()
}

impl MidiUnit {
    /// Send the soft key. The unit then leaves USB MIDI and comes back, a few
    /// seconds later, as the download-mode disk.
    pub fn soft_key(&self) -> Result<(), String> {
        sys::send(&self.port, &SOFT_KEY).map_err(|e| format!("cannot send to {} over USB MIDI: {e}", self.name))
    }
}

/// The first port of each unit among the system's MIDI output ports (by
/// name). A unit with several ports lists them under one name: the first is
/// its own ("... Private" on the SMK-37; Windows names the others
/// "MIDIOUT2 (...)", ...), and only that one answers the soft key.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn first_ports(names: &[String]) -> Vec<usize> {
    let mut chosen: Vec<(String, usize, u8)> = vec![];
    for (i, name) in names.iter().enumerate() {
        if !known(name) {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        let rank = if lower.contains("private") {
            0
        } else if !lower.starts_with("midiout") {
            1
        } else {
            2
        };
        let unit = NAMES.iter().find(|k| lower.contains(&k.to_ascii_lowercase())).unwrap().to_string();
        match chosen.iter_mut().find(|c| c.0 == unit) {
            Some(c) if rank < c.2 => *c = (unit, i, rank),
            Some(_) => {}
            None => chosen.push((unit, i, rank)),
        }
    }
    chosen.into_iter().map(|c| c.1).collect()
}

/// The sound cards in /proc/asound/cards: (number, name).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn asound_cards(text: &str) -> Vec<(u32, String)> {
    // " 3 [SMK37Elite     ]: USB-Audio - SMK-37 Elite"
    text.lines()
        .filter_map(|l| {
            let (num, rest) = l.trim_start().split_once(' ')?;
            let n = num.parse().ok()?;
            let name = rest.split_once("]: ")?.1;
            Some((n, name.split_once(" - ").map(|(_, n)| n).unwrap_or(name).trim().to_string()))
        })
        .collect()
}

#[cfg(target_os = "linux")]
mod sys {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    pub struct Port {
        card: u32,
    }

    pub fn units() -> Vec<(String, Port)> {
        let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
        super::asound_cards(&cards)
            .into_iter()
            .filter(|(n, name)| super::known(name) && std::path::Path::new(&format!("/dev/snd/midiC{n}D0")).exists())
            .map(|(card, name)| (name, Port { card }))
            .collect()
    }

    // _IOW('U', 0x42, int): which subdevice (port) this process's next open
    // of the card's raw-MIDI device gets, as `amidi -p hw:N,0,0` sets it
    const PREFER_SUBDEVICE: libc::c_ulong = 0x4004_5542;

    pub fn send(p: &Port, msg: &[u8]) -> std::io::Result<()> {
        let control = std::fs::OpenOptions::new().read(true).write(true).open(format!("/dev/snd/controlC{}", p.card));
        if let Ok(c) = &control {
            let port: libc::c_int = 0;
            // SAFETY: an ioctl taking a pointer to an int, on an open control device
            unsafe { libc::ioctl(c.as_raw_fd(), PREFER_SUBDEVICE as _, &port) };
        }
        let mut midi = std::fs::OpenOptions::new().write(true).open(format!("/dev/snd/midiC{}D0", p.card))?;
        midi.write_all(msg)?;
        midi.flush()?;
        drop(control);
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod sys {
    use midir::{MidiOutput, MidiOutputPort};

    pub struct Port(MidiOutputPort);

    /// CoreMIDI keeps the process's list of devices up to date through
    /// notifications on the run loop of the thread that first made a MIDI
    /// client: the window's scan thread, which runs no run loop. Without this,
    /// the list stays as it was at the first scan, and a unit connected later is
    /// never seen.
    #[cfg(target_os = "macos")]
    fn refresh() {
        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            static kCFRunLoopDefaultMode: *const std::ffi::c_void;
            fn CFRunLoopRunInMode(mode: *const std::ffi::c_void, seconds: f64, after_one: u8) -> i32;
        }
        // SAFETY: runs this thread's run loop until nothing is pending; the
        // mode is CoreFoundation's own constant
        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.0, 0) };
    }

    #[cfg(windows)]
    fn refresh() {}

    pub fn units() -> Vec<(String, Port)> {
        let Ok(out) = MidiOutput::new("M-Flasher") else { return vec![] };
        refresh();
        let ports = out.ports();
        let names: Vec<String> = ports.iter().map(|p| out.port_name(p).unwrap_or_default()).collect();
        super::first_ports(&names).into_iter().map(|i| (names[i].clone(), Port(ports[i].clone()))).collect()
    }

    pub fn send(p: &Port, msg: &[u8]) -> Result<(), String> {
        let out = MidiOutput::new("M-Flasher").map_err(|e| e.to_string())?;
        let mut conn = out.connect(&p.0, "soft key").map_err(|e| e.to_string())?;
        conn.send(msg).map_err(|e| e.to_string())?;
        conn.close();
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod sys {
    pub struct Port;
    pub fn units() -> Vec<(String, Port)> {
        vec![]
    }
    pub fn send(_: &Port, _: &[u8]) -> Result<(), String> {
        Err("USB MIDI is not supported on this system".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn one_port_per_unit() {
        // Windows: the first port bears the device's name
        let w = names(&["Microsoft GS Wavetable Synth", "MIDIOUT2 (SMK-37 Elite)", "SMK-37 Elite", "MIDIOUT3 (SMK-37 Elite)"]);
        assert_eq!(first_ports(&w), vec![2]);
        // ALSA-style names: the Private port
        let l = names(&["SMK-37 Pro-Master", "SMK-37 Pro-Private", "SMK-37 Pro-MCP", "FM-1 MIDI 1"]);
        assert_eq!(first_ports(&l), vec![1, 3]);
        assert!(first_ports(&names(&["Launchpad", "MIDI Through Port-0"])).is_empty());
    }

    #[test]
    fn proc_asound_cards() {
        let t = " 0 [PCH            ]: HDA-Intel - HDA Intel PCH\n                      HDA Intel PCH at 0xf7f10000 irq 32\n \
                 3 [SMK37Elite     ]: USB-Audio - SMK-37 Elite\n                      M-VAVE SMK-37 Elite at usb-0000:00:14.0-2, full speed\n";
        assert_eq!(asound_cards(t), vec![(0, "HDA Intel PCH".to_string()), (3, "SMK-37 Elite".to_string())]);
        assert!(known("SMK-37 Elite") && known("FM-1") && known("Felucca Update") && !known("HDA Intel PCH"));
    }

    #[test]
    fn soft_key_bytes() {
        assert_eq!(SOFT_KEY, [0xF0, 0x22, 0x24, 0x35, 0x7D, 0xF7]);
    }
}
