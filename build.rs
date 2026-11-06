//! Builds the vendored libopus into the program, on every target, with nothing but a C compiler.
//! On Windows targets, also gives the exe its icon: draws it from the same picture the program
//! uses, compiles it with windres, and links the resource. A missing windres only costs the icon.

#![allow(dead_code)]

include!("src/app/icon_pixels.rs");

use std::path::PathBuf;
use std::process::Command;

const OPUS_DIR: &str = "third_party/opus";

/// The files a variable names in one of libopus's `*_sources.mk` lists.
fn listed_sources(makefile: &str, variable: &str) -> Vec<String> {
    let text = std::fs::read_to_string(format!("{OPUS_DIR}/{makefile}")).expect("read the source list");
    let mut lines = text.lines().skip_while(|line| !line.starts_with(&format!("{variable} =")));
    let mut files = Vec::new();
    for line in lines.by_ref() {
        let continues = line.trim_end().ends_with('\\');
        files.extend(
            line.trim_end_matches('\\').split_whitespace().filter(|word| word.ends_with(".c")).map(String::from),
        );
        if !continues {
            break;
        }
    }
    assert!(!files.is_empty(), "{variable} is missing from {makefile}");
    files
}

/// The floating-point build, in plain C. libopus's cmake file is the usual way in, but this needs
/// no cmake and is the same on both targets. Optimised even in a debug build, so that tests run in
/// real time.
fn build_opus() {
    println!("cargo:rerun-if-changed={OPUS_DIR}");
    let lists = [
        ("celt_sources.mk", "CELT_SOURCES"),
        ("silk_sources.mk", "SILK_SOURCES"),
        ("silk_sources.mk", "SILK_SOURCES_FLOAT"),
        ("opus_sources.mk", "OPUS_SOURCES"),
        ("opus_sources.mk", "OPUS_SOURCES_FLOAT"),
    ];
    let mut build = cc::Build::new();
    for (makefile, variable) in lists {
        for file in listed_sources(makefile, variable) {
            build.file(format!("{OPUS_DIR}/{file}"));
        }
    }
    for dir in ["", "include", "celt", "silk", "silk/float"] {
        build.include(format!("{OPUS_DIR}/{dir}"));
    }
    for define in ["OPUS_BUILD", "FLOATING_POINT", "VAR_ARRAYS", "HAVE_LRINT", "HAVE_LRINTF"] {
        build.define(define, None);
    }
    build.define("PACKAGE_VERSION", "\"1.5.2\"");
    build.opt_level(3).warnings(false).compile("opus");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/app/icon_pixels.rs");
    build_opus();
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
