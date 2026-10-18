fn main() {
    assert!(std::env::var("LIBSQLITE3_FLAGS").unwrap_or_default().split_whitespace().any(|flag|flag=="-DSQLCIPHER_OMIT_LOG"),"Build from the application directory or source docs/tooling/m0/env.ps1: SQLCipher internal logging must be compiled out.");
    println!("cargo:rerun-if-changed=native/isolation.cpp");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        cc::Build::new().cpp(true).file("native/isolation.cpp").flag("/utf-8").compile("lvjin_isolation");
        println!("cargo:rustc-link-lib=userenv");
        println!("cargo:rustc-link-lib=advapi32");
    }
    #[cfg(feature="desktop")]
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&["app_status", "touch", "vault_create", "vault_unlock", "vault_lock", "vault_restore", "change_passphrase", "run_skill", "start_skill", "tasks", "task_cancel", "create_object", "import_files", "resume_import", "commit_change", "export_commit", "backup", "privacy", "view", "connections", "connection_save", "connection_probe", "connection_revoke", "connection_default", "answer_preview", "answer_start", "answer_discard", "generation_requests"]))).expect("desktop build resources failed");
}
