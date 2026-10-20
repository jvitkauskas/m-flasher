//! Download mode over USB MIDI: the soft key.
//!
//! The firmwares answer the SysEx `F0 22 24 35 7D F7` on their first MIDI port
//! by restarting into the chip's USB download mode: M-VAVE's stock firmware on
//! the FM-1 (015), the SMK-37 Pro (016) and the SMK-37 Elite (016), and
//! Felucca. So a unit that is running normally, even with the stock bootloader,
//! can be reached without holding any keys.
//!
//! Linux sends through the kernel's ALSA sequencer interface (`/dev/snd/seq`),
//! which shares the port with PipeWire, browsers and DAWs, with libc only, so
//! that the program needs no ALSA library; without a sequencer, to the raw-MIDI
//! device (`/dev/snd/midiC<card>D0`, port 0). Windows and macOS go through
//! midir (WinMM, CoreMIDI).

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
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    pub struct Port {
        card: i32,
        name: String,
    }

    pub fn units() -> Vec<(String, Port)> {
        let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
        super::asound_cards(&cards)
            .into_iter()
            .filter(|(n, name)| super::known(name) && std::path::Path::new(&format!("/dev/snd/midiC{n}D0")).exists())
            .map(|(card, name)| (name.clone(), Port { card: card as i32, name }))
            .collect()
    }

    /// Through the ALSA sequencer, which shares the port with everything else
    /// using it (PipeWire's MIDI bridge, browsers, DAWs); the raw-MIDI device
    /// gives the port to one program at a time. Raw MIDI only without it.
    pub fn send(p: &Port, msg: &[u8]) -> Result<(), String> {
        match seq::send(p.card, &p.name, msg) {
            Err(seq::Error::Unavailable) => raw_send(p.card, msg),
            other => other.map_err(|e| e.to_string()),
        }
    }

    // _IOW('U', 0x42, int): which subdevice (port) this process's next open
    // of the card's raw-MIDI device gets, as `amidi -p hw:N,0,0` sets it
    const PREFER_SUBDEVICE: libc::c_ulong = 0x4004_5542;

    fn raw_send(card: i32, msg: &[u8]) -> Result<(), String> {
        let control = OpenOptions::new().read(true).write(true).open(format!("/dev/snd/controlC{card}"));
        if let Ok(c) = &control {
            let port: libc::c_int = 0;
            // SAFETY: an ioctl taking a pointer to an int, on an open control device
            unsafe { libc::ioctl(c.as_raw_fd(), PREFER_SUBDEVICE as _, &port) };
        }
        // non-blocking: a port another program holds is refused at once
        // (blocking, the open would wait until it lets go)
        let mut midi = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(format!("/dev/snd/midiC{card}D0"))
            .map_err(|e| match e.raw_os_error() {
                Some(libc::EBUSY) => "its MIDI port is in use by another program: close it and try again".to_string(),
                _ => e.to_string(),
            })?;
        midi.write_all(msg).map_err(|e| e.to_string())?;
        drop(control);
        Ok(())
    }

    /// The kernel's sequencer interface (/dev/snd/seq): the layouts and
    /// requests of <sound/asequencer.h>.
    pub(super) mod seq {
        use super::*;
        use std::mem::size_of;

        #[repr(C)]
        #[derive(Clone, Copy, Default)]
        struct Addr {
            client: u8,
            port: u8,
        }

        #[repr(C)]
        struct ClientInfo {
            client: i32,
            kind: i32,
            name: [u8; 64],
            filter: u32,
            multicast_filter: [u8; 8],
            event_filter: [u8; 32],
            num_ports: i32,
            event_lost: i32,
            card: i32,
            pid: i32,
            midi_version: u32,
            group_filter: u32,
            reserved: [u8; 48],
        }

        #[repr(C)]
        struct PortInfo {
            addr: Addr,
            name: [u8; 64],
            capability: u32,
            kind: u32,
            midi_channels: i32,
            midi_voices: i32,
            synth_voices: i32,
            read_use: i32,
            write_use: i32,
            kernel: *mut libc::c_void,
            flags: u32,
            time_queue: u8,
            direction: u8,
            ump_group: u8,
            reserved: [u8; 57],
        }

        #[repr(C)]
        struct Subscribe {
            sender: Addr,
            dest: Addr,
            voices: u32,
            flags: u32,
            queue: u8,
            pad: [u8; 3],
            reserved: [u8; 64],
        }

        const _: () = assert!(size_of::<ClientInfo>() == 188 && size_of::<PortInfo>() == 168 && size_of::<Subscribe>() == 80);

        const fn ioc(dir: libc::c_ulong, nr: libc::c_ulong, size: usize) -> libc::c_ulong {
            (dir << 30) | ((size as libc::c_ulong) << 16) | (0x53 << 8) | nr
        }
        const W: libc::c_ulong = 1;
        const R: libc::c_ulong = 2;
        const CLIENT_ID: libc::c_ulong = ioc(R, 0x01, size_of::<libc::c_int>());
        const SET_CLIENT_INFO: libc::c_ulong = ioc(W, 0x11, size_of::<ClientInfo>());
        const CREATE_PORT: libc::c_ulong = ioc(R | W, 0x20, size_of::<PortInfo>());
        const SUBSCRIBE_PORT: libc::c_ulong = ioc(W, 0x30, size_of::<Subscribe>());
        const QUERY_NEXT_CLIENT: libc::c_ulong = ioc(R | W, 0x51, size_of::<ClientInfo>());
        const QUERY_NEXT_PORT: libc::c_ulong = ioc(R | W, 0x52, size_of::<PortInfo>());

        const KERNEL_CLIENT: i32 = 2;
        const CAP_READ: u32 = 1 << 0;
        const CAP_WRITE: u32 = 1 << 1;
        const CAP_SUBS_READ: u32 = 1 << 5;
        const CAP_SUBS_WRITE: u32 = 1 << 6;
        const TYPE_MIDI_GENERIC: u32 = 1 << 1;
        const TYPE_APPLICATION: u32 = 1 << 20;
        const EVENT_SYSEX: u8 = 130;
        const LENGTH_VARIABLE: u8 = 1 << 2;
        const QUEUE_DIRECT: u8 = 253;
        const SUBSCRIBERS: u8 = 254;

        pub enum Error {
            /// No sequencer, or no sequencer port for the card.
            Unavailable,
            Failed(String),
        }

        impl std::fmt::Display for Error {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                match self {
                    Error::Unavailable => f.write_str("no ALSA sequencer port"),
                    Error::Failed(s) => f.write_str(s),
                }
            }
        }

        fn zeroed<T>() -> T {
            // SAFETY: plain C structs of integers, arrays and a null pointer
            unsafe { std::mem::zeroed() }
        }

        fn ioctl<T>(f: &File, request: libc::c_ulong, arg: &mut T) -> std::io::Result<()> {
            // SAFETY: arg is the struct the request is defined with
            match unsafe { libc::ioctl(f.as_raw_fd(), request as _, arg as *mut T) } {
                0.. => Ok(()),
                _ => Err(std::io::Error::last_os_error()),
            }
        }

        fn text(b: &[u8]) -> String {
            String::from_utf8_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]).into_owned()
        }

        /// The first writable port of the card's kernel client: the unit's
        /// first MIDI port.
        fn unit_port(f: &File, card: i32, name: &str) -> Option<Addr> {
            let mut c: ClientInfo = zeroed();
            c.client = -1;
            while ioctl(f, QUERY_NEXT_CLIENT, &mut c).is_ok() {
                // (kernels before 4.6 report no card)
                if c.kind == KERNEL_CLIENT && (c.card == card || (c.card < 0 && text(&c.name) == name)) {
                    let mut p: PortInfo = zeroed();
                    p.addr = Addr { client: c.client as u8, port: 255 }; // the next after 255 is 0
                    while ioctl(f, QUERY_NEXT_PORT, &mut p).is_ok() {
                        if p.capability & (CAP_WRITE | CAP_SUBS_WRITE) == CAP_WRITE | CAP_SUBS_WRITE {
                            return Some(p.addr);
                        }
                    }
                }
            }
            None
        }

        fn open() -> Result<File, Error> {
            OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC)
                .open("/dev/snd/seq")
                .map_err(|_| Error::Unavailable)
        }

        pub fn send(card: i32, name: &str, msg: &[u8]) -> Result<(), Error> {
            let f = open()?;
            let dest = unit_port(&f, card, name).ok_or(Error::Unavailable)?;
            send_to(&f, dest, msg)
        }

        /// Send msg to the sequencer port client:port (for the loopback test).
        #[cfg(test)]
        pub fn send_to_port(client: u8, port: u8, msg: &[u8]) -> Result<(), Error> {
            send_to(&open()?, Addr { client, port }, msg)
        }

        fn send_to(f: &File, dest: Addr, msg: &[u8]) -> Result<(), Error> {
            let failed = |what: &str, e: std::io::Error| Error::Failed(format!("{what}: {e}"));
            let mut me: libc::c_int = 0;
            ioctl(f, CLIENT_ID, &mut me).map_err(|e| failed("sequencer client", e))?;
            let mut info: ClientInfo = zeroed();
            info.client = me;
            info.name[..9].copy_from_slice(b"M-Flasher");
            let _ = ioctl(f, SET_CLIENT_INFO, &mut info); // only a name for others to see
            let mut port: PortInfo = zeroed();
            port.addr = Addr { client: me as u8, port: 0 };
            port.name[..9].copy_from_slice(b"M-Flasher");
            port.capability = CAP_READ | CAP_SUBS_READ;
            port.kind = TYPE_MIDI_GENERIC | TYPE_APPLICATION;
            ioctl(f, CREATE_PORT, &mut port).map_err(|e| failed("sequencer port", e))?;
            // subscribing opens the unit's port for output, or joins those
            // already sending to it
            let mut sub: Subscribe = zeroed();
            sub.sender = port.addr;
            sub.dest = dest;
            ioctl(f, SUBSCRIBE_PORT, &mut sub).map_err(|e| match e.raw_os_error() {
                Some(libc::EBUSY) => {
                    Error::Failed("its MIDI port is in use by another program: close it and try again".into())
                }
                _ => failed("connecting to its MIDI port", e),
            })?;
            // the event (struct snd_seq_event, 28 bytes) and its data after it
            let mut ev = vec![0u8; 28];
            ev[0] = EVENT_SYSEX;
            ev[1] = LENGTH_VARIABLE;
            ev[3] = QUEUE_DIRECT;
            ev[12] = me as u8;
            ev[13] = port.addr.port;
            ev[14] = SUBSCRIBERS;
            ev[16..20].copy_from_slice(&(msg.len() as u32).to_ne_bytes());
            ev.extend_from_slice(msg);
            (&*f).write_all(&ev).map_err(|e| failed("sending", e))?;
            // closing unsubscribes; the kernel sends what is left first
            Ok(())
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod sys {
    use midir::{MidiOutput, MidiOutputPort};

    pub struct Port(MidiOutputPort);

    pub fn units() -> Vec<(String, Port)> {
        let Ok(out) = MidiOutput::new("M-Flasher") else { return vec![] };
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

    /// On Linux with a sequencer: `aseqdump` (from alsa-utils) shows its port,
    /// then `MIDI_TEST_PORT=client:port cargo test --release -- --ignored
    /// sequencer_loopback`; aseqdump prints the system exclusive F0 22 24 35 7D F7.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn sequencer_loopback() {
        let port = std::env::var("MIDI_TEST_PORT").expect("MIDI_TEST_PORT=client:port");
        let (c, p) = port.split_once(':').expect("client:port");
        match sys::seq::send_to_port(c.parse().unwrap(), p.parse().unwrap(), &SOFT_KEY) {
            Ok(()) => {}
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn soft_key_bytes() {
        assert_eq!(SOFT_KEY, [0xF0, 0x22, 0x24, 0x35, 0x7D, 0xF7]);
    }
}
