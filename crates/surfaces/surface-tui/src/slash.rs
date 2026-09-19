//! Slash commands (`05`, `06`): a line typed as `/name args…` in a surface's prompt box runs a
//! command the app registered instead of reaching the model. One [`Commands`] registry serves
//! every surface (this TUI, and the web UI through `surface-web`), so a command means the same
//! thing wherever it is typed. `/help` lists the registered commands; an unknown name is refused
//! with that list and nothing is sent to the model. A line starting with `//` is a prompt that
//! begins with `/`.
//!
//! Every run is logged by the operator as the pair `Command/Run{name, args}` /
//! `Command/Done{kind}` (`06`, `10` "commands are paired log entries"); `/help` and refusals are
//! answered without touching the log.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use aho_log::EntryKind;
use aho_session::Session;
use aho_types::{EntryId, Principal};

/// Runs one command against the session with the text after its name (trimmed, maybe empty) and
/// the command's own `Command/Run` entry (the authority a policy change cites), and returns the
/// line the operator reads, or an operator-facing refusal.
pub type Handler = Arc<dyn Fn(&Arc<Session>, &str, EntryId) -> Result<String, String> + Send + Sync>;

/// The built-in command every registry answers.
pub const HELP: &str = "help";

#[derive(Clone)]
struct Command {
    usage: String,
    help: String,
    run: Handler,
}

/// The commands a surface dispatches `/name` lines to, filled by the app.
#[derive(Clone, Default)]
pub struct Commands {
    by_name: BTreeMap<String, Command>,
}

impl fmt::Debug for Commands {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.by_name.keys()).finish()
    }
}

/// What a `/…` line came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The command ran (or `/help` answered); show this to the operator.
    Output(String),
    /// No command has this name; the text names the registered ones.
    Unknown(String),
    /// The command ran and refused, or could not be logged; the text says why.
    Failed(String),
}

impl Reply {
    pub fn text(&self) -> &str {
        match self {
            Reply::Output(t) | Reply::Unknown(t) | Reply::Failed(t) => t,
        }
    }
}

/// A prompt-box line, split into a command or text for the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parsed<'a> {
    Command { name: &'a str, args: &'a str },
    Prompt(&'a str),
}

/// `/name args…` is a command (`args` trimmed); `//text` is the prompt `/text`; anything else is a
/// prompt. The line is trimmed first.
pub fn parse_line(line: &str) -> Parsed<'_> {
    let line = line.trim();
    if line.starts_with("//") {
        return Parsed::Prompt(&line[1..]);
    }
    match line.strip_prefix('/') {
        Some(rest) => {
            let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            Parsed::Command { name, args: args.trim() }
        }
        None => Parsed::Prompt(line),
    }
}

impl Commands {
    pub fn new() -> Commands {
        Commands::default()
    }

    /// Registers `/name`. `usage` is the argument synopsis `/help` shows (e.g. `[reason]`, may be
    /// empty), `help` one line saying what it does.
    pub fn register(&mut self, name: &str, usage: &str, help: &str, run: Handler) -> Result<(), String> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
            return Err(format!("`/{name}` is not a command name; nothing was registered. Use lowercase letters, digits, `-` or `_`"));
        }
        if name == HELP || self.by_name.contains_key(name) {
            return Err(format!("`/{name}` is already a command; the second registration was refused. Pick another name"));
        }
        self.by_name.insert(name.to_string(), Command { usage: usage.trim().to_string(), help: help.to_string(), run });
        Ok(())
    }

    /// Registered names, sorted, without the built-in `help`.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// One line per command, `/help` first: `/name usage — help`.
    pub fn help_text(&self) -> String {
        let mut lines = vec![format!("/{HELP} — list these commands")];
        for (name, c) in &self.by_name {
            let usage = if c.usage.is_empty() { String::new() } else { format!(" {}", c.usage) };
            lines.push(format!("/{name}{usage} — {}", c.help));
        }
        lines.join("\n")
    }

    fn listing(&self) -> String {
        std::iter::once(HELP).chain(self.names()).map(|n| format!("/{n}")).collect::<Vec<_>>().join(", ")
    }

    /// Runs `/name args`: `help` lists, an unknown name is refused, anything else is logged as
    /// `Command/Run` + `Command/Done` around its handler.
    pub fn dispatch(&self, session: &Arc<Session>, name: &str, args: &str) -> Reply {
        if name == HELP {
            return Reply::Output(self.help_text());
        }
        let Some(c) = self.by_name.get(name) else {
            return Reply::Unknown(format!(
                "`/{name}` is not a command here, so nothing was sent to the model. Commands: {}. To send a line starting with `/` to the model, begin it with `//`",
                self.listing()
            ));
        };
        let log = session.log();
        let logged_args = if args.is_empty() { Vec::new() } else { vec![args.to_string()] };
        let run = match log.append(
            None,
            Principal::Operator,
            EntryKind::CommandRun { name: name.to_string(), args: logged_args, key: None },
        ) {
            Ok(e) => e.id,
            Err(e) => {
                return Reply::Failed(format!(
                    "`/{name}` was not run: the session log refused the command entry ({e}). Check that the session directory is writable, then retry"
                ));
            }
        };
        let ran = (c.run)(session, args, run);
        let kind = if ran.is_ok() { "ok" } else { "error" };
        let done = log.append(None, Principal::Operator, EntryKind::CommandDone { run, kind: kind.into() });
        match (ran, done) {
            (Ok(out), Ok(_)) => Reply::Output(out),
            (Ok(out), Err(e)) => Reply::Output(format!("{out} (the end of `/{name}` could not be logged: {e})")),
            (Err(why), _) => Reply::Failed(format!("`/{name}`: {why}")),
        }
    }
}
