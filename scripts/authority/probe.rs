//! Compiled by probe.py, not linked into any product crate.
#![deny(clippy::disallowed_methods)]
#![forbid(unsafe_code)]

mod host_provider {
    // Mirrors the audited HostRead/HostWrite adapter boundary. No exemption is
    // inherited by the sibling consumer, even when it calls this provider.
    #[allow(clippy::disallowed_methods, reason = "authority: fixture-host-io")]
    pub fn roundtrip(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
        std::fs::write(path, b"granted-provider-only")?;
        let bytes = std::fs::read(path)?;
        std::fs::remove_file(path)?;
        Ok(bytes)
    }
}

#[cfg(authority_probe = "fs_alias")]
fn denied() {
    use std::fs::read as harmless_name;
    drop(harmless_name("secret"));
}

#[cfg(authority_probe = "fs_function_pointer")]
fn denied() {
    let hidden: fn(&'static str) -> std::io::Result<Vec<u8>> = std::fs::read;
    drop(hidden("secret"));
}

#[cfg(authority_probe = "file_open")]
fn denied() {
    use std::fs::OpenOptions as Options;
    drop(Options::new().read(true).open("secret"));
}

#[cfg(authority_probe = "path_metadata")]
fn denied() {
    let exists = std::path::Path::new("secret").exists();
    std::hint::black_box(exists);
}

#[cfg(authority_probe = "process")]
fn denied() {
    type Innocent = std::process::Command;
    drop(Innocent::new("true").status());
}

#[cfg(authority_probe = "network")]
fn denied() {
    use std::net::TcpStream as Connection;
    drop(Connection::connect("127.0.0.1:9"));
}

#[cfg(authority_probe = "environment")]
fn denied() {
    use std::env::var_os as setting;
    drop(setting("HOME"));
}

#[cfg(authority_probe = "environment_iteration")]
fn denied() {
    drop(std::env::vars_os());
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Pure path construction and address parsing must remain available to consumers.
    let path = std::path::Path::new(&std::env::args_os().nth(1).ok_or("missing probe output path")?).to_path_buf();
    let address: std::net::SocketAddr = "127.0.0.1:9".parse()?;
    assert!(address.ip().is_loopback());
    assert_eq!(host_provider::roundtrip(&path)?, b"granted-provider-only");
    println!("declared provider filesystem roundtrip passed");
    Ok(())
}
