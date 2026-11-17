//! Embeds the app icon and version info (from Cargo.toml) in the exe.

use std::path::{Path, PathBuf};

#[path = "src/logo.rs"]
#[allow(dead_code)]
mod logo;

/// .ico with 32-bit BMP entries made from the logo.
fn write_ico(path: &Path, sizes: &[u32]) {
    let mut images = Vec::new();
    for &s in sizes {
        let pm = logo::draw_logo(s, 0.03, None);
        let rgba = logo::rgba(&pm);
        let mut img = Vec::new();
        // BITMAPINFOHEADER (height doubled: color + AND mask)
        for v in [40u32, s, s * 2] {
            img.extend_from_slice(&v.to_le_bytes());
        }
        img.extend_from_slice(&1u16.to_le_bytes());
        img.extend_from_slice(&32u16.to_le_bytes());
        for _ in 0..6 {
            img.extend_from_slice(&0u32.to_le_bytes());
        }
        for y in (0..s).rev() {
            for x in 0..s {
                let i = ((y * s + x) * 4) as usize;
                img.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
            }
        }
        let mask_row = s.div_ceil(32) * 4;
        img.extend(std::iter::repeat_n(0u8, (mask_row * s) as usize));
        images.push((s, img));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (s, img) in &images {
        let b = if *s >= 256 { 0 } else { *s as u8 };
        out.extend_from_slice(&[b, b, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for (_, img) in &images {
        out.extend_from_slice(img);
    }
    std::fs::write(path, out).expect("write icon");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/logo.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = out.join("app.ico");
    write_ico(&ico, &[16, 24, 32, 48, 64, 128, 256]);

    let ver = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut nums: Vec<String> = ver.split('.').map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect()).collect();
    nums.resize(4, "0".into());
    let n = nums.join(",");
    let rc = format!(
        r#"1 ICON "{ico}"
1 VERSIONINFO
FILEVERSION {n}
PRODUCTVERSION {n}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "Claude Usage"
      VALUE "ProductName", "Claude Usage"
      VALUE "FileVersion", "{ver}"
      VALUE "ProductVersion", "{ver}"
      VALUE "OriginalFilename", "ClaudeUsage-{ver}.exe"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        ico = ico.display().to_string().replace('\\', "/")
    );
    let rc_path = out.join("app.rc");
    std::fs::write(&rc_path, rc).expect("write rc");
    match embed_resource::compile(&rc_path, embed_resource::NONE) {
        embed_resource::CompilationResult::Ok | embed_resource::CompilationResult::NotWindows => {}
        embed_resource::CompilationResult::NotAttempted(why) => {
            println!("cargo:warning=exe built without icon/version info: {why}")
        }
        embed_resource::CompilationResult::Failed(why) => panic!("resource compile failed: {why}"),
    }
}
