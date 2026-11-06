//! On Windows targets, gives the exe its icon: draws it from the same picture the program uses,
//! compiles it with windres, and links the resource. A missing windres only costs the icon.

#![allow(dead_code)]

include!("src/app/icon_pixels.rs");

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/app/icon_pixels.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("gnu") {
        println!("cargo:warning=mesh-kvm: only the GNU Windows target gets an embedded icon");
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let ico_path = out.join("mesh-kvm.ico");
    let rc = out.join("mesh-kvm.rc");
    let object = out.join("mesh-kvm-icon.o");
    std::fs::write(&ico_path, ico()).expect("write the icon");
    std::fs::write(&rc, "1 ICON \"mesh-kvm.ico\"\n").expect("write the resource script");
    let windres = std::env::var("WINDRES").unwrap_or_else(|_| "x86_64-w64-mingw32-windres".into());
    let status = Command::new(&windres)
        .current_dir(&out)
        .args(["-O", "coff", "-i"])
        .arg(&rc)
        .arg("-o")
        .arg(&object)
        .status();
    match status {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", object.display()),
        Ok(s) => println!("cargo:warning=mesh-kvm: {windres} failed ({s}); the exe has no icon"),
        Err(e) => println!("cargo:warning=mesh-kvm: couldn't run {windres} ({e}); the exe has no icon"),
    }
}
