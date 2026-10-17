fn main() {
    assert!(std::env::var("LIBSQLITE3_FLAGS").unwrap_or_default().split_whitespace().any(|flag|flag=="-DSQLCIPHER_OMIT_LOG"),"Build from the application directory or source docs/tooling/m0/env.ps1: SQLCipher internal logging must be compiled out.");
    println!("cargo:rerun-if-changed=native/isolation.cpp");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        cc::Build::new().cpp(true).file("native/isolation.cpp").flag("/utf-8").compile("lvjin_isolation");
        println!("cargo:rustc-link-lib=userenv");
        println!("cargo:rustc-link-lib=advapi32");
    }
    #[cfg(feature="desktop")]
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&["installation_status"]))).expect("desktop build resources failed");
}
