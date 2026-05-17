//! TUI palette. Resolved once from `[settings]` + env, then borrowed by the
//! wizard renderer and the install reporter. Core stays terminal-agnostic
//! (it only carries preset name + hex strings); the name→Color mapping and
//! the env conventions live here, at the presentation edge.

use insmaller_core::{Settings, ThemeColors};
use ratatui::style::Color;

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub accent: Color,
    pub accent_fg: Color,
    pub muted: Color,
    pub error: Color,
}

impl Palette {
    fn preset(name: &str) -> Palette {
        match name {
            "mono" => Palette {
                accent: Color::Reset,
                accent_fg: Color::Reset,
                muted: Color::Reset,
                error: Color::Reset,
            },
            "high-contrast" => Palette {
                accent: Color::White,
                accent_fg: Color::Black,
                muted: Color::Gray,
                error: Color::LightRed,
            },
            // "default" and anything unrecognized
            _ => Palette {
                accent: Color::Cyan,
                accent_fg: Color::Black,
                muted: Color::DarkGray,
                error: Color::Red,
            },
        }
    }

    /// Precedence: `NO_COLOR` (presence ⇒ mono, per no-color.org; overrides
    /// ignored) → `INSMALLER_THEME` → `[settings] theme` → `default`. Hex
    /// `[settings] colors` overrides are layered on last (skipped under mono).
    pub fn resolve(settings: &Settings) -> Palette {
        let env_theme = std::env::var("INSMALLER_THEME").ok().filter(|s| !s.is_empty());
        Self::resolve_with(
            std::env::var_os("NO_COLOR").is_some(),
            env_theme.as_deref(),
            settings,
        )
    }

    /// Pure core of `resolve` (env already read by the caller) — keeps the
    /// precedence logic unit-testable without mutating process env.
    fn resolve_with(no_color: bool, env_theme: Option<&str>, settings: &Settings) -> Palette {
        if no_color {
            return Palette::preset("mono");
        }
        let name = env_theme.or(settings.theme.as_deref()).unwrap_or_default();
        let mut pal = Palette::preset(name);
        if let Some(c) = settings.colors.as_ref().filter(|_| pal.colored()) {
            pal.apply(c);
        }
        pal
    }

    /// True unless the palette is the no-color (`mono`/`NO_COLOR`) one.
    pub fn colored(&self) -> bool {
        !matches!(self.accent, Color::Reset)
    }

    fn apply(&mut self, c: &ThemeColors) {
        for (slot, hex) in [
            (&mut self.accent, &c.accent),
            (&mut self.accent_fg, &c.accent_fg),
            (&mut self.muted, &c.muted),
            (&mut self.error, &c.error),
        ] {
            if let Some(col) = hex.as_deref().and_then(parse_hex) {
                *slot = col;
            } else if let Some(bad) = hex.as_deref() {
                eprintln!("insmaller: ignoring invalid theme color '{bad}' (want #rrggbb)");
            }
        }
    }
}

/// `#rrggbb` → `Color::Rgb`. `None` on any malformed input (caller keeps the
/// preset value and warns).
fn parse_hex(s: &str) -> Option<Color> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(h, 16).ok()?;
    Some(Color::Rgb(
        (n >> 16) as u8,
        (n >> 8) as u8,
        n as u8,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(theme: Option<&str>, colors: Option<ThemeColors>) -> Settings {
        Settings {
            theme: theme.map(String::from),
            colors,
            ..Settings::default()
        }
    }

    #[test]
    fn parse_hex_accepts_only_well_formed() {
        assert_eq!(parse_hex("#ff8800"), Some(Color::Rgb(0xff, 0x88, 0x00)));
        assert_eq!(parse_hex("ff8800"), None); // no '#'
        assert_eq!(parse_hex("#fff"), None); // short
        assert_eq!(parse_hex("#gg0000"), None); // non-hex
    }

    #[test]
    fn unknown_preset_falls_back_to_default() {
        let p = Palette::preset("nope");
        assert_eq!(p.accent, Color::Cyan);
        assert_eq!(p.error, Color::Red);
    }

    #[test]
    fn no_color_forces_mono_and_ignores_overrides() {
        let s = settings(
            Some("high-contrast"),
            Some(ThemeColors {
                accent: Some("#ff0000".into()),
                ..Default::default()
            }),
        );
        let p = Palette::resolve_with(true, Some("default"), &s);
        assert_eq!(p.accent, Color::Reset);
        assert!(!p.colored());
    }

    #[test]
    fn env_theme_beats_config_theme() {
        let p = Palette::resolve_with(false, Some("mono"), &settings(Some("high-contrast"), None));
        assert_eq!(p.accent, Color::Reset);
    }

    #[test]
    fn config_theme_used_when_no_env() {
        let p = Palette::resolve_with(false, None, &settings(Some("high-contrast"), None));
        assert_eq!(p.accent, Color::White);
    }

    #[test]
    fn hex_overrides_layer_on_preset() {
        let s = settings(
            Some("default"),
            Some(ThemeColors {
                accent: Some("#123456".into()),
                error: Some("bogus".into()), // kept from preset, warns
                ..Default::default()
            }),
        );
        let p = Palette::resolve_with(false, None, &s);
        assert_eq!(p.accent, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(p.error, Color::Red); // invalid override ⇒ preset retained
    }
}
