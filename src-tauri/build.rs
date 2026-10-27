fn main() {
    assert!(std::env::var("LIBSQLITE3_FLAGS").unwrap_or_default().split_whitespace().any(|flag|flag=="-DSQLCIPHER_OMIT_LOG"),"Set LIBSQLITE3_FLAGS=-DSQLCIPHER_OMIT_LOG (see README.md): SQLCipher internal logging must be compiled out.");
    // SQLCipher's crypto provider: a static OpenSSL 3.5 LTS libcrypto from OPENSSL_DIR
    // (libsqlite3-sys links it; its Windows system dependencies are added here).
    let openssl=std::env::var_os("OPENSSL_DIR").map(std::path::PathBuf::from).expect("Set OPENSSL_DIR to a static OpenSSL 3.5 LTS build (see README.md).");
    assert!(openssl.join("lib/libcrypto.lib").is_file()&&openssl.join("include/openssl/opensslv.h").is_file(),"OPENSSL_DIR has no static libcrypto.lib and headers");
    println!("cargo:rerun-if-env-changed=OPENSSL_DIR");
    println!("cargo:rerun-if-changed=native/isolation.cpp");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        cc::Build::new().cpp(true).file("native/isolation.cpp").flag("/utf-8").compile("lvjin_isolation");
        println!("cargo:rustc-link-lib=userenv");
        println!("cargo:rustc-link-lib=advapi32");
        // Static libcrypto's system libraries, for every binary and test of
        // this package (also those that use rusqlite without the core library).
        for library in ["advapi32","crypt32","ws2_32","user32","gdi32"] { println!("cargo:rustc-link-lib={library}"); println!("cargo:rustc-link-arg={library}.lib"); }
    }
    #[cfg(feature="desktop")]
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&["app_status","touch","vault_create","vault_unlock","vault_lock","vault_restore","change_passphrase","run_skill","start_skill","tasks","task_cancel","task_retry","create_object","feedback_remove","import_files","resume_import","export_commit","backup","privacy","view","connections","connection_save","connection_probe","connection_revoke","connection_default","qa_history","generation_requests","index_jobs","index_resume","personal_query","personal_action","chat_send","chat_approve"]))).expect("desktop build resources failed");
}
