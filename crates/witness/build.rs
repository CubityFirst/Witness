//! Windows resources for `witness.exe`: the app icon (resource ID 32512,
//! the same ID tauri-build used),
//! VERSIONINFO (the xtask checks `ProductVersion` before packaging) and the
//! application manifest (Common Controls v6, PerMonitorV2, long paths).

fn main() {
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=witness.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let mut res = winresource::WindowsResource::new();
    res.set_icon_with_id("icons/icon.ico", "32512")
        .set("ProductName", "Witness")
        .set("FileDescription", "Witness")
        .set("CompanyName", "github")
        .set("FileVersion", &version)
        .set("ProductVersion", &version)
        .set_manifest_file("witness.manifest");
    res.compile().expect("compile Windows resources");
}
