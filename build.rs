//! Embeds the app icon and version info in the exe, so Explorer and Task
//! Manager show the icon and app name instead of a bare "prune.exe".

#[path = "src/icon.rs"]
#[allow(dead_code)]
mod icon;
#[path = "src/png.rs"]
#[allow(dead_code)]
mod png;

use std::path::{Path, PathBuf};

const APP_NAME: &str = "Prune";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = out.join("app.ico");
    write_ico(&ico, &[16, 20, 24, 32, 40, 48, 64, 256]);

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico.to_str().unwrap());
    res.set("FileDescription", APP_NAME);
    res.set("ProductName", APP_NAME);
    res.set("InternalName", APP_NAME);
    if let Err(e) = res.compile() {
        // A missing Windows SDK shouldn't break the build; the exe just has no icon.
        println!("cargo:warning=no exe icon/version info: {e}");
    }
}

/// ICO with one PNG image per size (supported since Windows Vista).
fn write_ico(path: &Path, sizes: &[u32]) {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| png::encode_rgba(s, s, &icon::rgba(s))).collect();
    let mut f = Vec::new();
    f.extend_from_slice(&[0, 0, 1, 0]);
    f.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (s, img) in sizes.iter().zip(&images) {
        let dim = if *s >= 256 { 0 } else { *s as u8 };
        f.extend_from_slice(&[dim, dim, 0, 0]);
        f.extend_from_slice(&1u16.to_le_bytes()); // planes
        f.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        f.extend_from_slice(&(img.len() as u32).to_le_bytes());
        f.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in &images {
        f.extend_from_slice(img);
    }
    std::fs::write(path, f).unwrap();
}
