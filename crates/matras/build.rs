use std::path::Path;
use std::process::Command;

// Bundle the Adwaita symbolic icons the UI names into the binary. On the
// desktop these come from the system icon theme; an Android APK has no such
// theme, so without this every toolbar button is a broken-image placeholder.
// Compiled into a gresource and registered at startup (see `run`).
//
// glib-compile-resources is invoked directly rather than through
// glib-build-tools: that crate pulls `glib` into the build-dependency graph,
// whose *-sys build scripts read the cross build's PKG_CONFIG_PATH and leak
// aarch64 link paths into the host build-script link. A bare command has no
// such graph.
fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let xml = "icons/matras.gresource.xml";
    let out = Path::new(&out_dir).join("matras-icons.gresource");

    let status = Command::new("glib-compile-resources")
        .arg("--sourcedir=icons")
        .arg("--target")
        .arg(&out)
        .arg(xml)
        .status()
        .expect("glib-compile-resources must be on PATH");
    assert!(status.success(), "glib-compile-resources failed");

    println!("cargo:rerun-if-changed={xml}");
    println!("cargo:rerun-if-changed=icons/scalable");
}
