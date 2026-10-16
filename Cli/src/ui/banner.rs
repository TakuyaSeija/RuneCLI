//! The logo banner.
//!
//! # The floating arcane crystal logo
//!
//! Replaces the old monochrome 3D ASCII text with the official Rune crystal
//! logo, rendered in truecolor half-blocks (`▀` and `▄`) at 5-6 vertical character
//! height matching the Antigravity Termux CLI layout.
//!
//! # Theme-aware
//!
//! Switches automatically with the active theme:
//! - Violet (Default): Cyber Violet arcane crystal with floating purple shards.
//! - Cyan: Arcane Cyan crystalline ice shards.
//! - Amber: Solar Amber golden sun crystal.

use super::ansi::{self, ACCENT, BOLD, FAINT, MUTED, RESET, TEXT};
use super::theme::ThemeMode;

const LOGO_W: usize = 11;
const LOGO_H: usize = 16;

const VIOLET_BYTES: &[u8] = include_bytes!("../../Assets/VioletLogo16.rgba");
const CYAN_BYTES: &[u8] = include_bytes!("../../Assets/CyanLogo16.rgba");
const GOLD_BYTES: &[u8] = include_bytes!("../../Assets/GoldLogo16.rgba");

fn logo_bytes_for(mode: ThemeMode) -> &'static [u8] {
    match mode {
        ThemeMode::Violet => VIOLET_BYTES,
        ThemeMode::Cyan => CYAN_BYTES,
        ThemeMode::Amber => GOLD_BYTES,
    }
}

/// Render the active logo banner into terminal lines using the app state.
pub fn render(app: &super::App, cols: usize) -> Vec<String> {
    let mode = app.theme;
    // The build this window runs, never its host's (see `App::host_version`).
    let version = runeengine::VERSION;
    let model = if app.model().is_empty() {
        "Select Models"
    } else {
        app.model()
    };
    let thinking = app.thinking();
    let effort = app.effort();
    let (mode_name, mode_ink) = super::draw::mode_of(&app.mode);
    let cwd = super::draw::home(&app.project);
    render_banner(
        cols, mode, version, model, thinking, effort, mode_name, mode_ink, &cwd,
    )
}

/// The official crystal, shared by the conversation and Settings.
pub fn logo(mode: ThemeMode) -> Vec<String> {
    let bytes = logo_bytes_for(mode);
    let h = LOGO_H;
    let w = LOGO_W;
    let mut out: Vec<String> = Vec::with_capacity(h / 2);

    for row in 0..h / 2 {
        let mut line = String::new();
        let y_top = row * 2;
        let y_bot = row * 2 + 1;

        for x in 0..w {
            let i1 = (y_top * w + x) * 4;
            let i2 = (y_bot * w + x) * 4;
            let (r1, g1, b1, a1) = (bytes[i1], bytes[i1 + 1], bytes[i1 + 2], bytes[i1 + 3]);
            let (r2, g2, b2, a2) = (bytes[i2], bytes[i2 + 1], bytes[i2 + 2], bytes[i2 + 3]);

            let t_on = a1 > 25;
            let b_on = a2 > 25;

            if t_on && b_on {
                line.push_str(&format!(
                    "{}{}▀{RESET}",
                    ansi::Rgb(r1, g1, b1),
                    ansi::encode(ansi::Rgb(r2, g2, b2), ansi::depth(), true)
                ));
            } else if t_on {
                line.push_str(&format!("{}▀{RESET}", ansi::Rgb(r1, g1, b1)));
            } else if b_on {
                line.push_str(&format!("{}▄{RESET}", ansi::Rgb(r2, g2, b2)));
            } else {
                line.push(' ');
            }
        }

        out.push(line);
    }
    out
}

/// Render the logo banner with custom parameters.
#[allow(clippy::too_many_arguments)] // Existing rendering contract keeps the caller responsible for its session data.
pub fn render_banner(
    cols: usize,
    mode: ThemeMode,
    version: &str,
    model: &str,
    thinking: bool,
    effort: Option<&str>,
    mode_name: &str,
    mode_ink: ansi::Ink,
    cwd: &str,
) -> Vec<String> {
    if cols < 30 {
        return vec![
            format!("  {BOLD}{TEXT}Rune CLI{RESET}  {FAINT}{version}{RESET}"),
            String::new(),
        ];
    }

    let mut out = Vec::new();
    for (row, pixels) in logo(mode).into_iter().enumerate() {
        let mut line = format!("  {pixels}");
        if cols >= 48 {
            let think_str = match super::thinking_label(thinking, effort) {
                Some(lbl) => format!(" {FAINT}{lbl}{RESET}"),
                None => String::new(),
            };
            match row {
                2 => line.push_str(&format!("    {ACCENT}{BOLD}Rune CLI{RESET}  {MUTED}{version}{RESET}")),
                3 => line.push_str(&format!("    {TEXT}{model}{RESET}{think_str} {FAINT}·{RESET} {mode_ink}{BOLD}{mode_name}{RESET}")),
                4 => line.push_str(&format!("    {MUTED}{cwd}{RESET}")),
                5 => line.push_str(&format!("    {FAINT}Quick:{RESET} {MUTED}/ commands{RESET} {FAINT}·{RESET} {MUTED}Tab complete{RESET} {FAINT}·{RESET} {MUTED}Esc cancel/stop{RESET} {FAINT}·{RESET} {MUTED}? help{RESET}")),
                _ => {}
            }
        }

        out.push(ansi::truncate(&line, cols));
    }
    out.push(String::new());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_renders_logo_cleanly() {
        for mode in ThemeMode::ALL {
            let lines = render_banner(
                100,
                mode,
                "0.1.0",
                "ag/gemini-3.8-flash-high",
                false,
                None,
                "Ask",
                ansi::OK,
                "~/Project/Clode",
            );
            assert_eq!(lines.len(), LOGO_H / 2 + 1);
            for line in &lines {
                assert!(ansi::width(line) <= 100);
            }
            assert!(lines.iter().any(|l| l.contains('▀') || l.contains('▄')));
            assert!(lines.iter().any(|l| l.contains("Rune CLI")));
            assert!(lines.iter().any(|l| l.contains("gemini-3.8-flash-high")));
        }
    }

    #[test]
    fn a_narrow_terminal_gets_a_line_rather_than_a_broken_drawing() {
        let lines = render_banner(
            20,
            ThemeMode::Violet,
            "0.1.0",
            "model",
            false,
            None,
            "Ask",
            ansi::OK,
            "~/path",
        );
        assert!(lines.iter().all(|l| ansi::width(l) <= 20));
        assert!(lines.join("").contains("Rune CLI"));
    }

    #[test]
    fn logo_alone_rendered_when_width_is_moderate() {
        let lines = render_banner(
            35,
            ThemeMode::Violet,
            "0.1.0",
            "model",
            false,
            None,
            "Ask",
            ansi::OK,
            "~/path",
        );
        assert_eq!(lines.len(), LOGO_H / 2 + 1);
        for line in &lines[..LOGO_H / 2] {
            assert_eq!(ansi::width(line), LOGO_W + 2);
        }
    }
}
