//! Builds the WASI layer (C) into a static library linked into the runtime.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let src = PathBuf::from("../wasi/wasi.c");
    println!("cargo:rerun-if-changed=../wasi/wasi.c");
    println!("cargo:rerun-if-changed=../wasi/wasi.h");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap();
    let msvc = target.contains("msvc");
    let cc = env::var("CC").unwrap_or_else(|_| "clang".into());
    let obj = out.join("wasi.o");
    let mut cmd = Command::new(&cc);
    cmd.args(["-c", "-O2", "-Wall", "-I../wasi"])
        .arg(&src)
        .arg("-o")
        .arg(&obj);
    if msvc {
        // Match the C runtime Rust links against (the DLL one).
        cmd.args(["-fms-runtime-lib=dll", "-D_CRT_SECURE_NO_WARNINGS"]);
    } else {
        cmd.arg("-fPIC");
    }
    let status = cmd.status().expect("could not run the C compiler (set CC)");
    assert!(status.success(), "compiling wasi.c failed");
    let lib = out.join(if msvc { "wasi.lib" } else { "libwasi.a" });
    let ar = env::var("AR").unwrap_or_else(|_| if msvc { "llvm-ar".into() } else { "ar".into() });
    let status = Command::new(&ar)
        .arg("crs")
        .arg(&lib)
        .arg(&obj)
        .status()
        .expect("could not run ar (set AR)");
    assert!(status.success(), "archiving wasi.o failed");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=wasi");
}
