//! The `/` menu: typing `/` in the prompt lists what can run, narrowed as you type. The TUI's own
//! views (`/tree`, `/jobs`, `/rewind`, …) come first, then the app's commands; after a command's
//! name and a space, the menu offers its argument's values when they are a closed set. Pure: the
//! list is a function of the draft and the command table, recomputed on every frame.

use crate::model::UiState;
use crate::slash::{CommandInfo, HELP};

/// The commands the TUI answers itself (they open views; nothing is logged).
pub const VIEWS: &[(&str, &str)] = &[
    (HELP, "commands, views and keys"),
    ("tree", "capabilities: state, why, and the fix"),
    ("inventory", "every capability card"),
    ("jobs", "operations of this session"),
    ("invariants", "invariant verdicts against the latest checkpoint"),
    ("rewind", "go back to a checkpoint: chat, code, or both"),
    ("hud", "show or hide entry ids and origins in the transcript"),
    ("quit", "leave the TUI; the session keeps its log"),
];

/// Rows the menu shows at once.
pub const MENU_ROWS: usize = 8;

/// One row of the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    /// What the row shows (`/tree`, or an argument value).
    pub label: String,
    /// The synopsis after the label (`<name>`), may be empty.
    pub usage: String,
    pub desc: String,
    /// The draft after accepting this row.
    pub insert: String,
    /// Enter on this row runs it (else Enter completes, like Tab, and waits for the argument).
    pub run: bool,
}

/// Every command the prompt accepts: the views, then the app's (a name a view already has is the
/// view's).
pub fn commands(ui: &UiState) -> Vec<CommandInfo> {
    let mut all: Vec<CommandInfo> =
        VIEWS.iter().map(|(n, h)| CommandInfo { name: (*n).into(), usage: String::new(), help: (*h).into(), choices: vec![] }).collect();
    for c in &ui.commands {
        if !all.iter().any(|v| v.name == c.name) {
            all.push(c.clone());
        }
    }
    all
}

/// How well `query` matches `name`: `None` = not at all; lower is better (prefix, then substring,
/// then the letters in order).
pub fn score(query: &str, name: &str) -> Option<usize> {
    let (q, n) = (query.to_ascii_lowercase(), name.to_ascii_lowercase());
    if q.is_empty() || n.starts_with(&q) {
        return Some(n.len());
    }
    if let Some(at) = n.find(&q) {
        return Some(100 + at);
    }
    let mut gaps = 0;
    let mut chars = n.chars();
    for qc in q.chars() {
        loop {
            match chars.next() {
                Some(c) if c == qc => break,
                Some(_) => gaps += 1,
                None => return None,
            }
        }
    }
    Some(200 + gaps)
}

/// The menu for the current draft, best first. Empty unless the draft is a `/` line being typed
/// at its end (a `//` line is a prompt).
pub fn suggestions(ui: &UiState) -> Vec<Suggestion> {
    let draft = &ui.local.draft;
    if !draft.starts_with('/') || draft.starts_with("//") || ui.local.cursor != draft.chars().count() {
        return Vec::new();
    }
    let rest = &draft[1..];
    let all = commands(ui);
    if let Some((name, arg)) = rest.split_once(char::is_whitespace) {
        let Some(c) = all.iter().find(|c| c.name == name) else { return Vec::new() };
        let arg = arg.trim_start();
        let mut hits: Vec<(usize, &(String, String))> = c.choices.iter().filter_map(|ch| score(arg, &ch.0).map(|s| (s, ch))).collect();
        hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.0.cmp(&b.1.0)));
        return hits
            .into_iter()
            .map(|(_, (value, desc))| Suggestion {
                label: value.clone(),
                usage: String::new(),
                desc: desc.clone(),
                insert: format!("/{name} {value}"),
                run: true,
            })
            .collect();
    }
    let mut hits: Vec<(usize, usize, &CommandInfo)> =
        all.iter().enumerate().filter_map(|(i, c)| score(rest, &c.name).map(|s| (s, i, c))).collect();
    // Prefix matches keep the table's order (views first); weaker matches rank by score.
    hits.sort_by(|a, b| (a.0 >= 100).cmp(&(b.0 >= 100)).then(if a.0 >= 100 { a.0.cmp(&b.0) } else { a.1.cmp(&b.1) }));
    hits.into_iter()
        .map(|(_, _, c)| {
            let takes_arg = !c.usage.is_empty();
            Suggestion {
                label: format!("/{}", c.name),
                usage: c.usage.clone(),
                desc: c.help.clone(),
                insert: if takes_arg { format!("/{} ", c.name) } else { format!("/{}", c.name) },
                run: !c.needs_args(),
            }
        })
        .collect()
}

/// The menu as shown: the rows and the highlighted index, or `None` when it is closed.
pub fn open(ui: &UiState) -> Option<(Vec<Suggestion>, usize)> {
    if ui.local.menu_dismissed || ui.covered() {
        return None;
    }
    let items = suggestions(ui);
    if items.is_empty() {
        return None;
    }
    let selected = ui.local.menu_selected.min(items.len() - 1);
    Some((items, selected))
}

/// Rows the menu takes on screen (0 when closed).
pub fn height(ui: &UiState) -> u16 {
    open(ui).map_or(0, |(items, _)| items.len().min(MENU_ROWS) as u16 + u16::from(items.len() > MENU_ROWS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui(draft: &str) -> UiState {
        let mut ui = UiState::default();
        ui.commands = vec![
            CommandInfo {
                name: "preset".into(),
                usage: "<name>".into(),
                help: "switch the policy".into(),
                choices: vec![("read-only".into(), "look only".into()), ("dev".into(), "+network".into()), ("lab".into(), "+VMs".into())],
            },
            CommandInfo { name: "plan".into(), usage: "[reason]".into(), help: "plan mode".into(), choices: vec![] },
        ];
        ui.local.draft = draft.into();
        ui.local.cursor = draft.chars().count();
        ui
    }

    #[test]
    fn a_slash_lists_views_then_commands_and_typing_narrows_by_prefix_then_letters() {
        let all: Vec<String> = suggestions(&ui("/")).into_iter().map(|s| s.label).collect();
        assert_eq!(all[..3], ["/help", "/tree", "/inventory"]);
        assert!(all.ends_with(&["/preset".to_string(), "/plan".to_string()]));
        let p: Vec<String> = suggestions(&ui("/p")).into_iter().map(|s| s.label).collect();
        assert_eq!(p, ["/preset", "/plan", "/help"], "prefix matches first, then `p` inside /help");
        let inv: Vec<String> = suggestions(&ui("/ivt")).into_iter().map(|s| s.label).collect();
        assert_eq!(inv, ["/inventory", "/invariants"], "letters in order");
        assert!(suggestions(&ui("//etc")).is_empty() && suggestions(&ui("hello")).is_empty());
    }

    #[test]
    fn a_command_that_needs_its_argument_completes_first_and_then_offers_its_values() {
        let preset = &suggestions(&ui("/pre"))[0];
        assert_eq!((preset.insert.as_str(), preset.run), ("/preset ", false), "Enter completes and waits for the name");
        let plan = &suggestions(&ui("/pla"))[0];
        assert_eq!((plan.insert.as_str(), plan.run), ("/plan ", true), "an optional argument runs without one");
        let values: Vec<String> = suggestions(&ui("/preset ")).into_iter().map(|s| s.insert).collect();
        assert_eq!(values, ["/preset dev", "/preset lab", "/preset read-only"]);
        let one = suggestions(&ui("/preset l"));
        assert_eq!(one[0].insert, "/preset lab");
        assert!(suggestions(&ui("/plan why")).is_empty(), "free text has no values to offer");
    }
}
