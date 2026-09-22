//! `aho-sdk` — the aho-sdk-min product as a binary, and the SDK generator (xtask-style).
//!
//! * `aho-sdk [--project DIR] [--set K=V]… [--disable ID]… rpc [--session HEX] [--memory]`:
//!   serves the rpc protocol on stdio for one session, exactly as `aho rpc` does for its product.
//! * `aho-sdk gen [--check] [--out DIR]`: writes the JSON Schema and the TS/Python SDK sources
//!   generated from the protocol table into `aho/sdk/` (or checks they are current; exit 1 if not).

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use aho_sdk::{Open, Sdk, codegen, protocol::PROTOCOL};
use aho_types::SessionId;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "aho-sdk", about = "The aho-sdk-min product (spine + rpc) and the TS/Python SDK generator.")]
struct Cli {
    /// Set a config key for this invocation (the Arg layer). Repeatable.
    #[arg(long = "set", value_name = "KEY=VALUE", global = true, value_parser = key_value)]
    sets: Vec<(String, String)>,
    /// Disable a capability for this invocation. Repeatable.
    #[arg(long = "disable", value_name = "ID", global = true)]
    disable: Vec<String>,
    /// Project directory (default: the current directory).
    #[arg(long, value_name = "DIR", global = true)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand, Debug)]
enum Verb {
    /// Serve the NDJSON rpc protocol on stdio for one session.
    Rpc {
        /// Resume this session (its hex id, as `aho session list` shows).
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Keep the log in memory only.
        #[arg(long, conflicts_with = "session")]
        memory: bool,
    },
    /// Write the JSON Schema and the TS/Python SDK sources generated from the protocol table.
    Gen {
        /// Only check that the committed files are current; exit 1 and list what differs.
        #[arg(long)]
        check: bool,
        /// SDK root (default: `aho/sdk` in this checkout).
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
    },
}

fn key_value(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .filter(|(k, _)| !k.trim().is_empty())
        .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        .ok_or_else(|| format!("`{s}` is not KEY=VALUE"))
}

async fn rpc(cli: &Cli, session: Option<&str>, memory: bool) -> Result<(), String> {
    let open = match session {
        Some(hex) => Open::Resume(SessionId(
            u128::from_str_radix(hex, 16)
                .map_err(|e| format!("session id `{hex}` is not hex ({e}); pass the id `aho session list` shows"))?,
        )),
        None if memory => Open::InMemory,
        None => Open::New,
    };
    let mut b = Sdk::builder().inherit_env();
    if let Some(p) = &cli.project {
        b = b.project(p);
    }
    for (k, v) in &cli.sets {
        b = b.set(k, v);
    }
    for id in &cli.disable {
        b = b.disable(id);
    }
    let sdk = b.build().map_err(|e| e.to_string())?;
    sdk.serve_io(open, tokio::io::stdin(), tokio::io::stdout()).await.map(|_| ()).map_err(|e| e.to_string())
}

fn generate(check: bool, out: Option<PathBuf>) -> Result<(), String> {
    let dir = out.unwrap_or_else(codegen::default_dir);
    let files = codegen::generate(&PROTOCOL);
    if check {
        let drift = codegen::drift(&dir, &files);
        if drift.is_empty() {
            eprintln!("{} generated files in {} are current", files.len(), dir.display());
            return Ok(());
        }
        return Err(format!("{}\nthe SDK sources are stale; run `cargo run -p aho-sdk -- gen`", drift.join("\n")));
    }
    let wrote = codegen::write(&dir, &files).map_err(|e| format!("could not write the SDK sources under {} ({e})", dir.display()))?;
    for p in &wrote {
        eprintln!("wrote {}", p.display());
    }
    eprintln!("{} of {} generated files changed", wrote.len(), files.len());
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.verb {
        Verb::Rpc { session, memory } => rpc(&cli, session.as_deref(), *memory).await,
        Verb::Gen { check, out } => generate(*check, out.clone()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
