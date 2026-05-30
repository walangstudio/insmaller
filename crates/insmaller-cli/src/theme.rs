//! TUI palette. Resolved once from `[settings]` + env, then borrowed by the
//! wizard renderer and the install reporter. Core stays terminal-agnostic
//! (it only carries preset name + hex strings); the name→Color mapping and
//! the env conventions live here, at the presentation edge.

use insmaller_core::{Settings, ThemeColors};
use ratatui::style::Color;

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub accent: Color,
    /// Gradient endpoint paired with `accent` (header sheen, progress fill).
    /// Equal to `accent` on flat presets so gradients render as a solid color.
    pub accent2: Color,
    pub accent_fg: Color,
    pub muted: Color,
    pub error: Color,
    /// Idle panel border.
    pub border: Color,
    /// Focused panel border (focus glow).
    pub border_focus: Color,
    /// Drop-shadow fill behind the modal.
    pub shadow: Color,
    pub success: Color,
}

/// `#rrggbb` → `Color::Rgb`, infallibly (literals are valid). For preset tables.
const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl Palette {
    fn preset(name: &str) -> Palette {
        match name {
            "mono" => Palette {
                accent: Color::Reset,
                accent2: Color::Reset,
                accent_fg: Color::Reset,
                muted: Color::Reset,
                error: Color::Reset,
                border: Color::Reset,
                border_focus: Color::Reset,
                shadow: Color::Reset,
                success: Color::Reset,
            },
            "high-contrast" => Palette {
                accent: Color::White,
                accent2: Color::White,
                accent_fg: Color::Black,
                muted: Color::Gray,
                error: Color::LightRed,
                border: Color::Gray,
                border_focus: Color::White,
                shadow: Color::Black,
                success: Color::LightGreen,
            },
            // Legacy flat cyan: gradient endpoints collapse to a solid color.
            "default" => Palette {
                accent: Color::Cyan,
                accent2: Color::Cyan,
                accent_fg: Color::Black,
                muted: Color::DarkGray,
                error: Color::Red,
                border: Color::DarkGray,
                border_focus: Color::Cyan,
                shadow: Color::Black,
                success: Color::Green,
            },
            // "modern" and anything unrecognized — midnight indigo→violet.
            _ => Palette {
                accent: rgb(0x6366f1),
                accent2: rgb(0xa855f7),
                accent_fg: rgb(0x0b1020),
                muted: rgb(0x64748b),
                error: rgb(0xfb7185),
                border: rgb(0x3b3f5c),
                border_focus: rgb(0x818cf8),
                shadow: rgb(0x11131c),
                success: rgb(0x34d399),
            },
        }
    }

    /// Precedence: `NO_COLOR` (presence ⇒ mono, per no-color.org; overrides
    /// ignored) → `INSMALLER_THEME` → `[settings] theme` → `modern` (default).
    /// Hex `[settings] colors` overrides are layered on last (skipped under
    /// mono).
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
            (&mut self.accent2, &c.accent2),
            (&mut self.accent_fg, &c.accent_fg),
            (&mut self.muted, &c.muted),
            (&mut self.error, &c.error),
            (&mut self.border, &c.border),
            (&mut self.border_focus, &c.border_focus),
            (&mut self.shadow, &c.shadow),
            (&mut self.success, &c.success),
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

/// `n` colors lerping `a`→`b` in RGB. If either endpoint isn't truecolor
/// (16-color preset, `mono`, `NO_COLOR`), returns `n` copies of `a` so the
/// gradient renders as a flat color and nothing regresses. `n == 0` ⇒ empty;
/// `n == 1` ⇒ just `a`.
pub fn gradient(a: Color, b: Color, n: usize) -> Vec<Color> {
    let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (a, b) else {
        return vec![a; n];
    };
    if n <= 1 {
        return vec![a; n];
    }
    let lerp = |x: u8, y: u8, t: f32| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (0..n)
        .map(|i| {
            let t = i as f32 / (n - 1) as f32;
            Color::Rgb(lerp(ar, br, t), lerp(ag, bg, t), lerp(ab, bb, t))
        })
        .collect()
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
    fn unknown_preset_falls_back_to_modern() {
        let p = Palette::preset("nope");
        assert_eq!(p.accent, Color::Rgb(0x63, 0x66, 0xf1));
        assert_eq!(p.error, Color::Rgb(0xfb, 0x71, 0x85));
        // The legacy cyan look is still reachable by its explicit name.
        let d = Palette::preset("default");
        assert_eq!(d.accent, Color::Cyan);
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
                accent2: Some("#abcdef".into()),
                border_focus: Some("#0f0f0f".into()),
                error: Some("bogus".into()), // kept from preset, warns
                ..Default::default()
            }),
        );
        let p = Palette::resolve_with(false, None, &s);
        assert_eq!(p.accent, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(p.accent2, Color::Rgb(0xab, 0xcd, 0xef));
        assert_eq!(p.border_focus, Color::Rgb(0x0f, 0x0f, 0x0f));
        assert_eq!(p.error, Color::Red); // invalid override ⇒ preset retained
    }

    #[test]
    fn modern_is_default_when_unset() {
        // No env, no [settings] theme ⇒ modern (truecolor indigo→violet).
        let p = Palette::resolve_with(false, None, &settings(None, None));
        assert_eq!(p.accent, Color::Rgb(0x63, 0x66, 0xf1));
        assert_eq!(p.accent2, Color::Rgb(0xa8, 0x55, 0xf7));
        assert!(p.colored());
    }

    #[test]
    fn mono_zeroes_new_roles() {
        let p = Palette::preset("mono");
        for c in [p.accent, p.accent2, p.border, p.border_focus, p.shadow, p.success] {
            assert_eq!(c, Color::Reset);
        }
        assert!(!p.colored());
    }

    #[test]
    fn gradient_endpoints_and_flat_for_non_rgb() {
        let g = gradient(Color::Rgb(0, 0, 0), Color::Rgb(10, 20, 40), 3);
        assert_eq!(g[0], Color::Rgb(0, 0, 0)); // first = a
        assert_eq!(g[2], Color::Rgb(10, 20, 40)); // last = b
        assert_eq!(g[1], Color::Rgb(5, 10, 20)); // midpoint lerp
        // A non-truecolor endpoint ⇒ flat (n copies of a), no panic.
        let flat = gradient(Color::Cyan, Color::Rgb(1, 2, 3), 4);
        assert_eq!(flat, vec![Color::Cyan; 4]);
        assert_eq!(gradient(Color::Rgb(1, 1, 1), Color::Rgb(2, 2, 2), 0), vec![]);
    }
}
