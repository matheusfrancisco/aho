//! Colours by role, not by value: every style the TUI draws comes from a [`Theme`], built once from
//! the terminal's capabilities (truecolor, 256, 16, or none at all under `NO_COLOR`) and its
//! background (dark or light). Structure never depends on colour: with colour off, selection
//! and emphasis still show through bold and reverse video.

use ratatui::style::{Color, Modifier, Style};

/// What the terminal can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    /// `NO_COLOR` (or a dumb terminal): attributes only.
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

impl ColorMode {
    /// From the environment: `NO_COLOR` wins, then `COLORTERM`, then `TERM`.
    pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> ColorMode {
        if env("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return ColorMode::None;
        }
        let term = env("TERM").unwrap_or_default();
        if term == "dumb" {
            return ColorMode::None;
        }
        if env("COLORTERM").is_some_and(|c| matches!(c.to_ascii_lowercase().as_str(), "truecolor" | "24bit")) {
            return ColorMode::TrueColor;
        }
        if term.contains("256color") || term.contains("kitty") || term.contains("ghostty") || term.contains("alacritty") {
            return ColorMode::Ansi256;
        }
        ColorMode::Ansi16
    }
}

/// One colour as each mode renders it.
#[derive(Clone, Copy)]
struct Swatch {
    rgb: (u8, u8, u8),
    idx: u8,
    basic: Color,
}

const fn sw(rgb: (u8, u8, u8), idx: u8, basic: Color) -> Swatch {
    Swatch { rgb, idx, basic }
}

impl Swatch {
    fn color(self, mode: ColorMode) -> Option<Color> {
        match mode {
            ColorMode::None => None,
            ColorMode::Ansi16 => Some(self.basic),
            ColorMode::Ansi256 => Some(Color::Indexed(self.idx)),
            ColorMode::TrueColor => Some(Color::Rgb(self.rgb.0, self.rgb.1, self.rgb.2)),
        }
    }
}

struct Palette {
    accent: Swatch,
    muted: Swatch,
    dim: Swatch,
    user: Swatch,
    success: Swatch,
    warning: Swatch,
    error: Swatch,
    info: Swatch,
    border: Swatch,
    selection_bg: Swatch,
    pending_bg: Swatch,
    ok_bg: Swatch,
    err_bg: Swatch,
    panel_bg: Swatch,
}

const DARK: Palette = Palette {
    accent: sw((95, 175, 255), 75, Color::Cyan),
    muted: sw((158, 158, 158), 247, Color::Gray),
    dim: sw((108, 108, 108), 242, Color::DarkGray),
    user: sw((135, 215, 255), 117, Color::LightCyan),
    success: sw((135, 215, 135), 114, Color::Green),
    warning: sw((255, 175, 95), 215, Color::Yellow),
    error: sw((255, 95, 95), 203, Color::Red),
    info: sw((215, 135, 255), 177, Color::Magenta),
    border: sw((78, 78, 78), 239, Color::DarkGray),
    selection_bg: sw((48, 58, 74), 237, Color::Blue),
    pending_bg: sw((38, 36, 26), 235, Color::Reset),
    ok_bg: sw((26, 36, 30), 234, Color::Reset),
    err_bg: sw((46, 28, 28), 52, Color::Reset),
    panel_bg: sw((28, 28, 32), 234, Color::Reset),
};

const LIGHT: Palette = Palette {
    accent: sw((0, 95, 175), 25, Color::Blue),
    muted: sw((98, 98, 98), 241, Color::DarkGray),
    dim: sw((138, 138, 138), 245, Color::Gray),
    user: sw((0, 95, 135), 24, Color::Blue),
    success: sw((0, 135, 0), 28, Color::Green),
    warning: sw((175, 95, 0), 130, Color::Yellow),
    error: sw((175, 0, 0), 124, Color::Red),
    info: sw((135, 0, 175), 91, Color::Magenta),
    border: sw((188, 188, 188), 250, Color::Gray),
    selection_bg: sw((215, 228, 245), 189, Color::Cyan),
    pending_bg: sw((250, 246, 228), 230, Color::Reset),
    ok_bg: sw((235, 247, 238), 194, Color::Reset),
    err_bg: sw((252, 232, 232), 224, Color::Reset),
    panel_bg: sw((246, 246, 248), 255, Color::Reset),
};

/// Styles by role. Construct with [`Theme::new`] or [`Theme::detect`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub mode: ColorMode,
    pub dark: bool,
    pub text: Style,
    pub accent: Style,
    pub title: Style,
    pub muted: Style,
    pub dim: Style,
    pub user: Style,
    pub thinking: Style,
    pub success: Style,
    pub warning: Style,
    pub error: Style,
    pub info: Style,
    pub border: Style,
    pub border_focus: Style,
    /// The highlighted row of a list (menu, picker).
    pub selected: Style,
    pub key: Style,
    /// Background tints of a tool card by its state (none below 256 colours).
    pub tool_pending: Style,
    pub tool_ok: Style,
    pub tool_err: Style,
    /// Behind a panel.
    pub panel: Style,
}

impl Default for Theme {
    fn default() -> Theme {
        Theme::new(ColorMode::Ansi256, true)
    }
}

impl Theme {
    pub fn new(mode: ColorMode, dark: bool) -> Theme {
        let p = if dark { &DARK } else { &LIGHT };
        let fg = |s: Swatch| s.color(mode).map_or(Style::default(), |c| Style::default().fg(c));
        // Tints only where they stay subtle: 16-colour backgrounds are too loud.
        let bg = |s: Swatch| match mode {
            ColorMode::Ansi256 | ColorMode::TrueColor => s.color(mode).map_or(Style::default(), |c| Style::default().bg(c)),
            _ => Style::default(),
        };
        let selected = match mode {
            ColorMode::None | ColorMode::Ansi16 => Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            _ => bg(p.selection_bg).patch(fg(p.accent)).add_modifier(Modifier::BOLD),
        };
        Theme {
            mode,
            dark,
            text: Style::default(),
            accent: fg(p.accent),
            title: fg(p.accent).add_modifier(Modifier::BOLD),
            muted: fg(p.muted),
            dim: fg(p.dim),
            user: fg(p.user).add_modifier(Modifier::BOLD),
            thinking: fg(p.dim).add_modifier(Modifier::ITALIC),
            success: fg(p.success),
            warning: fg(p.warning),
            error: fg(p.error),
            info: fg(p.info),
            border: fg(p.border),
            border_focus: fg(p.accent),
            selected,
            key: fg(p.accent).add_modifier(Modifier::BOLD),
            tool_pending: bg(p.pending_bg),
            tool_ok: bg(p.ok_bg),
            tool_err: bg(p.err_bg),
            panel: bg(p.panel_bg),
        }
    }

    /// From the environment: colour depth as [`ColorMode::detect`]; the background from
    /// `AHO_THEME=dark|light`, else `COLORFGBG` (`fg;bg`, bg 7 or 15 is light), else dark.
    pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> Theme {
        let dark = match env("AHO_THEME").as_deref().map(str::trim) {
            Some("light") => false,
            Some("dark") => true,
            _ => env("COLORFGBG")
                .and_then(|v| v.rsplit(';').next().and_then(|b| b.trim().parse::<u8>().ok()))
                .is_none_or(|bg| !matches!(bg, 7 | 15)),
        };
        Theme::new(ColorMode::detect(env), dark)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn no_color_wins_over_every_capability_and_keeps_selection_visible() {
        let t = Theme::detect(&env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor"), ("TERM", "xterm-256color")]));
        assert_eq!(t.mode, ColorMode::None);
        assert_eq!(t.accent, Style::default(), "no colour at all");
        assert!(t.selected.add_modifier.contains(Modifier::REVERSED), "the selected row still shows");
        assert_eq!(Theme::detect(&env(&[("TERM", "dumb")])).mode, ColorMode::None);
    }

    #[test]
    fn depth_follows_colorterm_then_term_and_background_follows_colorfgbg() {
        assert_eq!(ColorMode::detect(&env(&[("COLORTERM", "24bit"), ("TERM", "xterm")])), ColorMode::TrueColor);
        assert_eq!(ColorMode::detect(&env(&[("TERM", "xterm-256color")])), ColorMode::Ansi256);
        assert_eq!(ColorMode::detect(&env(&[("TERM", "xterm")])), ColorMode::Ansi16);
        assert!(!Theme::detect(&env(&[("COLORFGBG", "0;15")])).dark);
        assert!(Theme::detect(&env(&[("COLORFGBG", "15;0")])).dark);
        assert!(!Theme::detect(&env(&[("AHO_THEME", "light"), ("COLORFGBG", "15;0")])).dark, "AHO_THEME overrides");
        let tc = Theme::detect(&env(&[("COLORTERM", "truecolor")]));
        assert_eq!(tc.accent.fg, Some(Color::Rgb(95, 175, 255)));
        assert!(Theme::new(ColorMode::Ansi16, true).tool_ok.bg.is_none(), "no loud 16-colour tints");
    }
}
