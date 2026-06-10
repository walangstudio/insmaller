//! `--version` / about-block rendering. Plain-stdout text with optional ANSI
//! styling, driven by `[project]` config fields and an optional
//! `version_template`. Lives in core (not the CLI) because minijinja — and
//! therefore the style filters — already lives here.

use crate::config::ProjectMeta;
use minijinja::{Environment, UndefinedBehavior};
use serde_json::{json, Map, Value};
use std::path::Path;

/// Named style → ANSI SGR code. Filters stack: each wraps its input in
/// `\x1b[<code>m … \x1b[0m`, so `{{ x | bold | cyan }}` is bold + cyan (the
/// codes accumulate before the text; the trailing resets are harmless).
const STYLES: &[(&str, u8)] = &[
    ("bold", 1),
    ("dim", 2),
    ("italic", 3),
    ("underline", 4),
    ("black", 30),
    ("red", 31),
    ("green", 32),
    ("yellow", 33),
    ("blue", 34),
    ("magenta", 35),
    ("cyan", 36),
    ("white", 37),
    ("gray", 90),
    ("grey", 90),
];

/// `#rrggbb` / `rrggbb` → `(r, g, b)`. `None` on malformed input so the
/// truecolor filters degrade to passing their text through unchanged.
fn parse_hex6(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(h, 16).ok()?;
    Some(((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

/// Best-effort read of just the `[project]` table from a config file, for
/// `--version`. Tolerant: `None` on any read/parse error so `--version` never
/// fails on a malformed-but-present config (only `[project]` is type-checked;
/// other tables are ignored).
pub fn probe_project_meta(path: &Path) -> Option<ProjectMeta> {
    #[derive(serde::Deserialize)]
    struct Probe {
        #[serde(default)]
        project: Option<ProjectMeta>,
    }
    let s = std::fs::read_to_string(path).ok()?;
    toml::from_str::<Probe>(&s).ok()?.project
}

/// Render the `--version` / about block. With `color == false` the style
/// filters pass their input through unchanged (for `NO_COLOR` and pipes).
/// Never fails: a missing or broken `version_template` falls back to
/// `"<program_name> <engine_version>"`.
pub fn render_about(
    project: Option<&ProjectMeta>,
    program_name: &str,
    engine_version: &str,
    color: bool,
) -> String {
    let version = project.and_then(|p| p.version.as_deref());
    let template = match project.and_then(|p| p.version_template.as_deref()) {
        Some(t) => t.to_owned(),
        None if version.is_some() => {
            "{{ name }} {{ version }} (insmaller {{ engine_version }})".to_owned()
        }
        None => "{{ name }} {{ engine_version }}".to_owned(),
    };

    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Lenient);
    for &(style, code) in STYLES {
        env.add_filter(style, move |v: String| {
            if color {
                format!("\x1b[{code}m{v}\x1b[0m")
            } else {
                v
            }
        });
    }
    // Truecolor: `{{ x | rgb("818cf8") }}` and a per-character
    // `{{ x | gradient("818cf8","f472b6") }}`. Both pass text through when
    // color is off or the hex is malformed. `gradient` sets a color per char
    // and resets once at the end, so chaining `| bold` keeps bold across the
    // whole run.
    env.add_filter("rgb", move |v: String, hex: String| {
        match (color, parse_hex6(&hex)) {
            (true, Some((r, g, b))) => format!("\x1b[38;2;{r};{g};{b}m{v}\x1b[0m"),
            _ => v,
        }
    });
    env.add_filter("gradient", move |v: String, from: String, to: String| {
        let (Some(a), Some(b)) = (parse_hex6(&from), parse_hex6(&to)) else {
            return v;
        };
        let chars: Vec<char> = v.chars().collect();
        if !color || chars.is_empty() {
            return v;
        }
        let n = chars.len();
        let mut out = String::new();
        for (i, ch) in chars.iter().enumerate() {
            let t = if n <= 1 { 0.0 } else { i as f32 / (n - 1) as f32 };
            let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            out.push_str(&format!(
                "\x1b[38;2;{};{};{}m",
                lerp(a.0, b.0),
                lerp(a.1, b.1),
                lerp(a.2, b.2)
            ));
            out.push(*ch);
        }
        out.push_str("\x1b[0m");
        out
    });

    let mut ctx: Map<String, Value> = Map::new();
    ctx.insert("name".into(), json!(program_name));
    ctx.insert("version".into(), json!(version.unwrap_or_default()));
    ctx.insert("engine_version".into(), json!(engine_version));
    ctx.insert(
        "about".into(),
        json!(project.and_then(|p| p.about.as_deref()).unwrap_or_default()),
    );
    ctx.insert(
        "copyright".into(),
        json!(project
            .and_then(|p| p.copyright.as_deref())
            .unwrap_or_default()),
    );
    ctx.insert(
        "extra".into(),
        json!(project.map(|p| p.extra.clone()).unwrap_or_default()),
    );

    env.render_str(&template, &ctx)
        .unwrap_or_else(|_| format!("{program_name} {engine_version}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pm(version: Option<&str>, tmpl: Option<&str>) -> ProjectMeta {
        ProjectMeta {
            version: version.map(String::from),
            version_template: tmpl.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn no_project_falls_back_to_engine_version() {
        assert_eq!(
            render_about(None, "codetainyrrr", "0.9.0", false),
            "codetainyrrr 0.9.0"
        );
    }

    #[test]
    fn version_without_template_uses_app_plus_engine() {
        let p = pm(Some("0.1.0"), None);
        assert_eq!(
            render_about(Some(&p), "codetainyrrr", "0.9.0", false),
            "codetainyrrr 0.1.0 (insmaller 0.9.0)"
        );
    }

    #[test]
    fn custom_template_renders_fields_and_extra() {
        let mut p = pm(
            Some("0.1.0"),
            Some("{{ name }} {{ version }} — {{ copyright }} [{{ extra.license }}]"),
        );
        p.copyright = Some("© 2026 walang.studio".into());
        p.extra.insert("license".into(), "MIT".into());
        assert_eq!(
            render_about(Some(&p), "codetainyrrr", "0.9.0", false),
            "codetainyrrr 0.1.0 — © 2026 walang.studio [MIT]"
        );
    }

    #[test]
    fn color_filters_noop_when_color_false() {
        let p = pm(Some("0.1.0"), Some("{{ name | bold | cyan }}"));
        assert_eq!(render_about(Some(&p), "ct", "0.9.0", false), "ct");
    }

    #[test]
    fn color_filter_emits_ansi_when_color_true() {
        let p = pm(Some("0.1.0"), Some("{{ name | bold }}"));
        assert_eq!(
            render_about(Some(&p), "ct", "0.9.0", true),
            "\x1b[1mct\x1b[0m"
        );
    }

    #[test]
    fn broken_template_falls_back() {
        let p = pm(Some("0.1.0"), Some("{{ unclosed "));
        assert_eq!(render_about(Some(&p), "ct", "0.9.0", false), "ct 0.9.0");
    }

    #[test]
    fn undefined_extra_is_empty_not_error() {
        let p = pm(Some("0.1.0"), Some("{{ name }}{{ extra.missing }}"));
        assert_eq!(render_about(Some(&p), "ct", "0.9.0", false), "ct");
    }

    #[test]
    fn rgb_filter_emits_truecolor_when_on_and_passes_through_off() {
        let p = pm(Some("0.1.0"), Some(r#"{{ name | rgb("818cf8") }}"#));
        assert_eq!(
            render_about(Some(&p), "ct", "0.10.0", true),
            "\x1b[38;2;129;140;248mct\x1b[0m"
        );
        assert_eq!(render_about(Some(&p), "ct", "0.10.0", false), "ct");
    }

    #[test]
    fn rgb_bad_hex_passes_through() {
        let p = pm(Some("0.1.0"), Some(r#"{{ name | rgb("nothex") }}"#));
        assert_eq!(render_about(Some(&p), "ct", "0.10.0", true), "ct");
    }

    #[test]
    fn gradient_colors_each_char_and_resets_once() {
        // "ab": first char = from (#000000), last = to (#0000ff).
        let p = pm(Some("0.1.0"), Some(r#"{{ name | gradient("000000","0000ff") }}"#));
        let out = render_about(Some(&p), "ab", "0.10.0", true);
        assert_eq!(out, "\x1b[38;2;0;0;0ma\x1b[38;2;0;0;255mb\x1b[0m");
        // off → plain.
        assert_eq!(render_about(Some(&p), "ab", "0.10.0", false), "ab");
    }

    #[test]
    fn gradient_then_bold_keeps_bold_across_run() {
        let p = pm(Some("0.1.0"), Some(r#"{{ name | gradient("000000","0000ff") | bold }}"#));
        let out = render_about(Some(&p), "ab", "0.10.0", true);
        assert!(out.starts_with("\x1b[1m"), "bold opens the run: {out:?}");
        assert!(out.ends_with("\x1b[0m"));
    }
}
