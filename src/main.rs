//! M-Flasher: back up, restore and reinstall the firmware of the M-VAVE FM-1
//! and SMK-37 Pro / Elite through the chip's USB download mode, and install
//! m-bootloader. Without arguments: the window; with arguments: the
//! command line.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod boards;
mod cli;
mod controller;
mod fonts;
mod formats;
mod jl;
mod ops;
mod scsi;
mod sectormap;
#[cfg(test)]
mod tests;
mod ui;
mod wordmark;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // macOS: the root helper the app starts to reach the unit
    #[cfg(target_os = "macos")]
    if args.first().map(String::as_str) == Some(scsi::macos::USB_HELPER) {
        std::process::exit(scsi::macos::usb_helper());
    }
    if !args.is_empty() {
        #[cfg(windows)]
        unsafe {
            // a windows-subsystem app has no console: use the one it was started from
            windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
        }
        std::process::exit(cli::run(args));
    }
    #[cfg(target_os = "linux")]
    if let Some(problem) = linux_display_problem() {
        eprintln!("cannot open the window: {problem}");
        std::process::exit(1);
    }
    if let Err(e) = ui::run() {
        eprintln!("cannot open the window: {e}");
        std::process::exit(1);
    }
}

/// Why the window cannot open on this Linux desktop, if it cannot: checked
/// first, as the windowing library would abort with a less helpful message.
#[cfg(target_os = "linux")]
fn linux_display_problem() -> Option<String> {
    let set = |v| std::env::var_os(v).is_some_and(|s| !s.is_empty());
    let sudo = if set("SUDO_USER") {
        " sudo hides the desktop session: install the udev rule and run M-Flasher without sudo (see README.txt)."
    } else {
        ""
    };
    if !set("WAYLAND_DISPLAY") && !set("DISPLAY") {
        return Some(format!("no desktop session (neither WAYLAND_DISPLAY nor DISPLAY is set).{sudo} The command line works without one: m-flasher --help"));
    }
    // X11 (no Wayland session): the keyboard library's X11 part, which a
    // Wayland desktop may not have installed
    let loads = |name: &str| {
        let name = std::ffi::CString::new(name).unwrap();
        // SAFETY: dlopen with a valid C string, closed again at once
        unsafe {
            let h = libc::dlopen(name.as_ptr(), libc::RTLD_LAZY);
            !h.is_null() && libc::dlclose(h) == 0
        }
    };
    if !set("WAYLAND_DISPLAY") && !loads("libxkbcommon-x11.so.0") && !loads("libxkbcommon-x11.so") {
        return Some(format!(
            "on X11 it needs libxkbcommon-x11 (Debian, Ubuntu: sudo apt install libxkbcommon-x11-0).{sudo}"
        ));
    }
    None
}
