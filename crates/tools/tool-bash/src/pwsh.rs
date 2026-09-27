//! Pwsh parity: `caps.tool-bash.shell = pwsh` runs the same two tools in PowerShell 7.
//!
//! The blast-radius classifier (`aho_policy::shell`) parses POSIX shell only. It cannot clear a
//! PowerShell command, so under pwsh every command is held at the most conservative level that
//! still runs: its `Catastrophic` matches are still denied, and everything else asks the operator
//! (a `justification` does not skip the ask, since nothing classified the command). The core's step-5
//! safety check still runs on the `command` text as it does for every tool.

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_log::OpKind;
use aho_policy::shell::{RiskLevel, assess, reflection_prompt};
use aho_types::*;
use serde_json::json;

use crate::{PRESENT, schema_bash};

pub const SHELL_KEY: &str = "caps.tool-bash.shell";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShellKind {
    /// `/bin/sh` (the tool keeps its historical name `bash`).
    #[default]
    Posix,
    /// PowerShell 7 (`pwsh` on PATH).
    Pwsh,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BashConfig {
    pub shell: ShellKind,
}

impl Config for BashConfig {
    const PREFIX: &'static str = "caps.tool-bash";
    fn keys() -> Vec<ConfigKey> {
        vec![ConfigKey::new(
            SHELL_KEY,
            Ty::String,
            Some(json!("bash")),
            "Shell behind `bash`/`bash_persistent`: `bash` (POSIX `/bin/sh`) or `pwsh` (PowerShell 7; needs `pwsh` on PATH). Under pwsh the blast-radius classifier cannot read commands, so every command asks the operator.",
        )]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        match l.str(SHELL_KEY).unwrap_or("bash") {
            "bash" => Ok(BashConfig { shell: ShellKind::Posix }),
            "pwsh" => Ok(BashConfig { shell: ShellKind::Pwsh }),
            _ => Err(ConfigError::Type { key: SHELL_KEY.into(), expected: "`bash` or `pwsh`" }),
        }
    }
}

/// argv of a one-shot command.
pub fn oneshot(command: &str) -> Vec<String> {
    ["pwsh", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", command].iter().map(|s| s.to_string()).collect()
}

/// argv of the long-lived process: commands arrive one line each on stdin.
pub fn persistent() -> Vec<String> {
    ["pwsh", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "-"].iter().map(|s| s.to_string()).collect()
}

/// One `bash_persistent` call for pwsh, on one line: the command travels base64-encoded (no
/// quoting, no multi-line continuation), runs with `Invoke-Expression` in the global scope so
/// `Set-Location` and variables persist, a parse error or throw becomes status 1 without ending
/// the process, and the sentinel line carries the status (`$LASTEXITCODE` of a native command first).
pub fn script(command: &str, sentinel: &str) -> String {
    let b64 = base64(command.as_bytes());
    format!(
        "$global:LASTEXITCODE = 0; $__aho_ok = $true; try {{ Invoke-Expression ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{b64}'))); $__aho_ok = $? }} catch {{ $__aho_ok = $false; Write-Error -ErrorRecord $_ }}; $__aho_rc = if ($global:LASTEXITCODE) {{ $global:LASTEXITCODE }} elseif (-not $__aho_ok) {{ 1 }} else {{ 0 }}; [Console]::Out.Write(\"`n{sentinel} $__aho_rc`n\")\n"
    )
}

pub(crate) fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = ((chunk[0] as u32) << 16) | ((*chunk.get(1).unwrap_or(&0) as u32) << 8) | (*chunk.get(2).unwrap_or(&0) as u32);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= chunk.len() { A[((n >> shift) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// Decision-chain step 3 under pwsh: `Catastrophic` still denies; everything else asks.
pub fn check_pwsh(args: &Value, view: &PolicyView) -> Option<Decision> {
    let command = args.get("command").and_then(Value::as_str)?;
    let risk = assess(command, &view.workspace, &view.home);
    if risk.level == RiskLevel::Catastrophic {
        return Some(Decision::Deny(reflection_prompt(&risk)));
    }
    Some(Decision::Ask(format!(
        "This session's shell is PowerShell (`{SHELL_KEY} = pwsh`). The blast-radius classifier reads only POSIX shells, so it cannot vouch for this command, and every pwsh command asks:\n\n{command}"
    )))
}

/// Registered instead of `TOOLS` under pwsh: same names, classes, schema and ops; PowerShell
/// descriptions and the conservative check.
pub static PWSH_TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "bash",
        "Run a PowerShell 7 command (`pwsh -NoProfile -NonInteractive -Command`) in the workspace (this session's shell is pwsh). Returns combined output and the exit code. Each call is a fresh process. Every command asks the operator first.",
        Consequence::SpawnsProcess,
    )
    .schema(schema_bash)
    .check(check_pwsh)
    .op(OpKind::Process, false, true)
    .present(PRESENT),
    ToolDecl::new(
        "bash_persistent",
        "Run PowerShell in one long-lived pwsh process: the location, variables and functions persist between calls. `exit` ends it; the next call starts a fresh one. Every command asks the operator first.",
        Consequence::SpawnsProcess,
    )
    .schema(schema_bash)
    .check(check_pwsh)
    .op(OpKind::Process, false, true)
    .present(PRESENT),
];
