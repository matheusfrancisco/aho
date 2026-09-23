//! `05` — the operator CLI. Every verb is a thin client of the `caps.*` verb set (surface-rpc) or
//! a projection over a session log (aho-inspect); none exists in this client only.
//!
//! Exit codes: 0 ok, 1 error, 2 plan or verify failure.

#![forbid(unsafe_code)]

mod approval;
mod auth;
mod boot;
mod cli;
#[allow(clippy::disallowed_methods, reason = "authority: cli-config")]
mod config;
#[allow(clippy::disallowed_methods, reason = "authority: cli-ctx")]
mod ctx;
#[allow(clippy::disallowed_methods, reason = "authority: cli-history")]
mod history;
#[allow(clippy::disallowed_methods, reason = "authority: cli-invariants")]
mod invariants;
#[allow(clippy::disallowed_methods, reason = "authority: cli-net")]
mod net;
#[allow(clippy::disallowed_methods, reason = "authority: cli-ops")]
mod ops;
mod record;
#[allow(clippy::disallowed_methods, reason = "authority: cli-rpc-bridge")]
mod rpc_bridge;
#[allow(clippy::disallowed_methods, reason = "authority: cli-sandbox")]
mod sandbox;
#[allow(clippy::disallowed_methods, reason = "authority: cli-selfdev")]
mod selfdev_cmd;
#[allow(clippy::disallowed_methods, reason = "authority: cli-serve")]
mod serve;
mod serve_host;
#[allow(clippy::disallowed_methods, reason = "authority: cli-session")]
mod session;
mod slash;
#[allow(clippy::disallowed_methods, reason = "authority: cli-spec")]
mod spec;
#[allow(clippy::disallowed_methods, reason = "authority: cli-surfaces")]
mod surfaces;
mod vm;

use std::process::ExitCode;

use clap::Parser;
use clap::error::ErrorKind;

use cli::{
    AddWhat, CapsVerb, CatalogWhat, CheckpointVerb, Cli, CredentialVerb, DoctorWhat, LoadoutVerb, PlanModeVerb, RalphVerb, ScheduleVerb,
    SessionVerb, Verb,
};
use ctx::{Ctx, Fail};

async fn dispatch(cli: Cli) -> Result<(), Fail> {
    // 12: a reloaded daemon learns where to signal readiness from its environment; take it before
    // anything else can spawn a process that would inherit it.
    let ready = if matches!(cli.verb, Verb::Serve(_)) { daemon::ready::take_ready_fd() } else { None };
    let ctx = Ctx::new(cli.global)?;
    match &cli.verb {
        Verb::Plan { patch, lock, strict, json } => config::plan(&ctx, patch.as_deref(), *lock, *strict, *json),
        Verb::Tree { id, json } => config::tree(&ctx, id.as_deref(), *json),
        Verb::Inventory { kind, json } => config::inventory(&ctx, kind.as_deref(), *json),
        Verb::Explain { subject, at, session, json } => config::explain(&ctx, subject, *at, session.as_deref(), *json),
        Verb::Why { id, json } => config::why(&ctx, id, *json),
        Verb::Catalog { what: CatalogWhat::Config } => config::catalog_config(&ctx),
        Verb::Enable { id } => config::switch(&ctx, "enable", id).await,
        Verb::Disable { id } => config::switch(&ctx, "disable", id).await,
        Verb::Allow { id } => config::switch(&ctx, "allow", id).await,
        Verb::Set { key, value } => config::set(&ctx, key, value).await,
        Verb::Preset { name } => config::preset(&ctx, name).await,
        Verb::Loadout { sub } => match sub {
            LoadoutVerb::Save { name } => config::loadout_save(&ctx, name),
            LoadoutVerb::Diff { a, b } => config::loadout_diff(&ctx, a, b.as_deref()),
            LoadoutVerb::Load { name } => config::loadout_load(&ctx, name),
            LoadoutVerb::List => config::loadout_list(&ctx),
        },
        Verb::Lock => config::lock(&ctx),
        Verb::Run(args) => session::run(&ctx, args).await,
        Verb::Session { sub } => match sub {
            SessionVerb::List { json } => session::list(&ctx, *json),
            SessionVerb::Show { id } => session::show(&ctx, id),
            SessionVerb::Verify { id, formal } => session::verify(&ctx, id.as_deref(), *formal).await,
            SessionVerb::Branches { id } => session::branches(&ctx, id.as_deref()),
            SessionVerb::Diff { a, b, session } => history::diff(&ctx, *a, *b, session.as_deref()),
            SessionVerb::Merge { id, reason } => history::merge(&ctx, id.as_deref(), reason.as_deref()).await,
            SessionVerb::Destroy { id } => history::destroy(&ctx, id.as_deref()).await,
            SessionVerb::Query { query, json } => session::query(&ctx, query, *json),
            SessionVerb::Index { rebuild, json } => session::index(&ctx, *rebuild, *json),
        },
        Verb::Sandbox { sub, session, json } => sandbox::run(&ctx, sub, session.as_deref(), *json).await,
        Verb::Export { src, dst, session } => sandbox::export(&ctx, src, dst, session.as_deref()).await,
        Verb::Net { sub, session, json } => net::run(&ctx, sub, session.as_deref(), *json).await,
        Verb::Checkpoints { session } => session::checkpoints(&ctx, session.as_deref()),
        Verb::Rewind { checkpoint, chat, code, session } => session::rewind(&ctx, checkpoint, *chat, *code, session.as_deref()).await,
        Verb::Shell { session, cmd } => session::shell(&ctx, session.as_deref(), cmd).await,
        Verb::Status { loud, session, json } => session::status(&ctx, *loud, session.as_deref(), *json),
        Verb::Rpc { socket, session } => session::rpc(&ctx, socket.as_deref(), session.as_deref()).await,
        Verb::Spec { sub } => spec::run(&ctx, sub.as_ref()).await,
        Verb::Credential { sub: CredentialVerb::Set { name } } => config::credential_set(&ctx, name),
        Verb::Login(args) => auth::login(&ctx, args).await,
        Verb::Logout { provider } => auth::logout(&ctx, provider.as_deref()).await,
        Verb::Add { what: AddWhat::Mcp { name, cmd, env, class } } => config::add_mcp(&ctx, name, cmd, env, class.as_deref()),
        Verb::Add { what: AddWhat::Recipe(r) } => vm::add_recipe(&ctx, r),
        Verb::Lease { sub, session, json } => vm::lease(&ctx, sub, session.as_deref(), *json).await,
        Verb::Media { sub } => vm::media(&ctx, sub),
        Verb::Apply { file, session } => ops::apply(&ctx, file, session.as_deref()).await,
        Verb::Serve(a) if a.stop => serve::stop(&ctx, a.socket.as_deref()).await,
        Verb::Serve(a) => serve::serve(&ctx, ready, a).await,
        Verb::Attach(a) => serve::attach(&ctx, a).await,
        Verb::Tui { session, headless, width, height } => surfaces::tui(&ctx, session.as_deref(), *headless, *width, *height).await,
        Verb::Web { session, bind, cert, key, insecure_loopback } => {
            surfaces::web(&ctx, session.as_deref(), bind, cert.as_deref(), key.as_deref(), *insecure_loopback).await
        }
        Verb::Cert { hosts, force } => surfaces::cert(&ctx, hosts, *force),
        Verb::Webhook { session, insecure_loopback } => surfaces::webhook(&ctx, session.as_deref(), *insecure_loopback).await,
        Verb::Acp => surfaces::acp(ctx.clone()).await,
        Verb::Selfdev { sub } => selfdev_cmd::run(&ctx, sub).await,
        Verb::Fork { session, at } => history::fork(&ctx, session.as_deref(), *at).await,
        Verb::Checkpoint { sub: CheckpointVerb::Export { checkpoint, branch, session } } => {
            history::export(&ctx, checkpoint, branch, session.as_deref())
        }
        Verb::Invariants { sub, session, json } => invariants::run(&ctx, sub.as_ref(), session.as_deref(), *json).await,
        Verb::Doctor { what: DoctorWhat::Provider { id, tier, json } } => ops::doctor_provider(&ctx, id, tier, *json).await,
        Verb::Doctor { what: DoctorWhat::Formal { install } } => ops::doctor_formal(&ctx, install.as_deref()),
        Verb::Doctor { what: DoctorWhat::Embed { install } } => ops::doctor_embed(&ctx, *install).await,
        Verb::Caps { sub: CapsVerb::Show { id, json } } => ops::caps_show(&ctx, id, *json),
        Verb::Schedule { sub: ScheduleVerb::List { session, json } } => ops::schedule_list(&ctx, session.as_deref(), *json),
        Verb::Schedule { sub: ScheduleVerb::Cancel { id, session } } => ops::schedule_cancel(&ctx, id, session.as_deref()).await,
        Verb::Ralph { sub: RalphVerb::Stop { id, reason, session } } => {
            ops::ralph_stop(&ctx, id.as_deref(), reason.as_deref(), session.as_deref()).await
        }
        Verb::PlanMode { sub: PlanModeVerb::On { reason, session } } => {
            ops::plan_mode_on(&ctx, reason.as_deref(), session.as_deref()).await
        }
        Verb::Gc { dry_run } => ops::gc(&ctx, *dry_run),
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let code = if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) { 0 } else { 1 };
            // clap renders usage errors and help itself; a failure to print leaves nothing to add.
            if e.print().is_err() {
                return ExitCode::from(1);
            }
            return ExitCode::from(code);
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("aho: could not start the async runtime ({e})");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(dispatch(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => {
            match &f {
                Fail::Error(m) | Fail::Plan(m) => eprintln!("aho: {m}"),
                Fail::Reported => {}
            }
            ExitCode::from(f.code())
        }
    }
}
