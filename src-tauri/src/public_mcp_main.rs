//! No CLI path or arbitrary command surface. A parent pipe supplies startup
//! authentication and closing that pipe ends the service.
use std::io::{BufRead, Write};
use zeroize::Zeroizing;
fn main() {
    if run().is_err() { eprintln!("public_mcp_start_failed"); std::process::exit(1); }
}
fn run() -> lvjin_core::Result<()> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut first = Zeroizing::new(String::new());
    input.read_line(&mut first)?;
    if first.len() > 1024 { return Err(lvjin_core::Error::Contract); }
    let config: serde_json::Value = serde_json::from_str(first.as_str())?;
    let token = Zeroizing::new(config["token"].as_str().ok_or(lvjin_core::Error::Contract)?.to_owned());
    let port = config["port"].as_u64().unwrap_or(0);
    if port > u16::MAX as u64 { return Err(lvjin_core::Error::Contract); }
    drop(config); drop(first); drop(input);
    let cancel = tokio_util::sync::CancellationToken::new();
    let ended = cancel.clone();
    std::thread::spawn(move || { let mut line = String::new(); let _ = std::io::stdin().read_line(&mut line); ended.cancel(); });
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(lvjin_core::public_mcp_server::serve(port as u16, token.as_str(), cancel, |port| {
        println!("{}", serde_json::json!({"endpoint":format!("http://127.0.0.1:{port}/mcp"),"status":"ready"}));
        let _ = std::io::stdout().flush();
    }))
}
