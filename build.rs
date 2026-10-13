// Windows: the icon, and a manifest asking for administrator rights (needed
// for SCSI pass-through to the download-mode disk) and declaring the app
// DPI-aware, as resources of the executable.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=windows/m-flasher.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = || {
        use embed_manifest::manifest::{DpiAwareness, ExecutionLevel};
        embed_manifest::new_manifest("M-Flasher")
            .requested_execution_level(ExecutionLevel::RequireAdministrator)
            .dpi_awareness(DpiAwareness::PerMonitorV2)
    };
    let mut res = winresource::WindowsResource::new();
    res.set_icon("windows/m-flasher.ico")
        .set_manifest(&manifest().to_string())
        .set("FileDescription", "M-Flasher")
        .set("ProductName", "M-Flasher");
    if let Err(e) = res.compile() {
        // the release builds (MSVC) need both; a cross-build without a resource
        // compiler (windres) gets the manifest alone
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            panic!("compiling the Windows resources: {e}");
        }
        println!("cargo:warning=no Windows resource compiler ({e}): the manifest only, no icon");
        embed_manifest::embed_manifest(manifest()).expect("embedding the manifest");
    }
}
