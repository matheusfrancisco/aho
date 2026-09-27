//! Process token, one-time cookie exchange, and the Host/Origin fence.

use std::net::SocketAddr;

use axum::http::{HeaderMap, header};

pub const COOKIE: &str = "aho_session";

/// `n` random bytes as hex, from the TLS provider's CSPRNG.
pub fn random_hex(n: usize) -> Result<String, String> {
    let mut buf = vec![0u8; n];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut buf)
        .map_err(|_| "the system random source failed; cannot mint a web token".to_string())?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Constant-time over the bytes so a token cannot be guessed byte by byte from timing. The
/// length is public (always 64 hex chars), so a length mismatch returns early.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

pub fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").map(str::trim)
}

pub fn cookie(h: &HeaderMap) -> Option<&str> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| kv.trim().strip_prefix(COOKIE).and_then(|r| r.strip_prefix('=')))
}

/// The `Host` values and `Origin`s this server answers to. A request naming another host is a
/// DNS-rebinding attempt; a foreign `Origin` is a cross-site request. Both are refused.
#[derive(Clone, Debug)]
pub struct Fence {
    pub hosts: Vec<String>,
    pub origins: Vec<String>,
}

impl Fence {
    pub fn new(bind: SocketAddr, tls: bool, extra: &[String]) -> Fence {
        let port = bind.port();
        let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string(), "[::1]".to_string()];
        if !bind.ip().is_unspecified() {
            names.push(match bind {
                SocketAddr::V4(a) => a.ip().to_string(),
                SocketAddr::V6(a) => format!("[{}]", a.ip()),
            });
        }
        let mut hosts: Vec<String> = names.iter().map(|n| format!("{n}:{port}")).collect();
        for h in extra {
            let h = h.to_ascii_lowercase();
            hosts.push(if h.contains(':') && !h.ends_with(']') { h } else { format!("{h}:{port}") });
        }
        hosts.sort();
        hosts.dedup();
        let scheme = if tls { "https" } else { "http" };
        let origins = hosts.iter().map(|h| format!("{scheme}://{h}")).collect();
        Fence { hosts, origins }
    }

    /// `Err(reason)` when the request must be refused.
    pub fn check(&self, h: &HeaderMap) -> Result<(), String> {
        let host = h.get(header::HOST).and_then(|v| v.to_str().ok()).map(str::to_ascii_lowercase);
        match host {
            Some(host) if self.hosts.contains(&host) => {}
            Some(host) => {
                return Err(format!(
                    "Host `{host}` is not one this server answers to ({}). Open one of those addresses, or start the server with that host allowed.",
                    self.hosts.join(", ")
                ));
            }
            None => return Err("the request has no Host header".into()),
        }
        if let Some(origin) = h.get(header::ORIGIN) {
            let origin = origin.to_str().unwrap_or("").to_ascii_lowercase();
            if !self.origins.contains(&origin) {
                return Err(format!("cross-origin request from `{origin}` refused; only this server's own pages may call its API"));
            }
        }
        Ok(())
    }
}
