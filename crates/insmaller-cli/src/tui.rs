//! ratatui wizard TUI: a persistent screen with a progress gauge/breadcrumb
//! header, a per-page body, and on-screen [◄ Back] [Next ►] [Quit] buttons —
//! navigable by Tab/←/→ AND shortcut keys (Esc=back, Enter=next, q/Ctrl-C
//! quit). Drives a pure `WizardSession`. Plus an indicatif reporter for the
//! install phase.

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use indicatif::{ProgressBar, ProgressStyle};
use insmaller_core::{Field, FieldType, Reporter, WizardSession};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph},
    Terminal,
};
use crate::theme::Palette;
use serde_json::{Map, Value};
use std::io::{self, Stdout};

/// Restores the terminal even on panic/early-return.
struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

enum Widget {
    Multi { choices: Vec<insmaller_core::Choice>, on: Vec<bool>, cur: usize },
    Single { choices: Vec<insmaller_core::Choice>, sel: Option<usize>, cur: usize },
    Toggle { on: bool },
    Input { buf: String, secret: bool },
}

fn init_widget(f: &Field, s: &WizardSession) -> Widget {
    let prior = s.answer_for(&f.id).cloned();
    match f.field_type {
        FieldType::Multiselect => {
            let choices = s.choices(f);
            let on = choices
                .iter()
                .map(|c| match &prior {
                    Some(Value::Array(a)) => a.iter().any(|v| v.as_str() == Some(&c.value)),
                    _ => c.default,
                })
                .collect();
            Widget::Multi { choices, on, cur: 0 }
        }
        FieldType::SingleSelect => {
            let choices = s.choices(f);
            let sel = match &prior {
                Some(Value::String(v)) => choices.iter().position(|c| &c.value == v),
                _ => None,
            };
            Widget::Single { choices, sel, cur: 0 }
        }
        FieldType::Toggle => Widget::Toggle {
            on: matches!(prior, Some(Value::Bool(true))),
        },
        _ => Widget::Input {
            buf: match prior {
                Some(Value::String(s)) => s,
                _ => f.default.clone().unwrap_or_default(),
            },
            secret: f.field_type == FieldType::Secret,
        },
    }
}

fn widget_value(w: &Widget) -> Value {
    match w {
        Widget::Multi { choices, on, .. } => Value::Array(
            choices
                .iter()
                .zip(on)
                .filter(|(_, &o)| o)
                .map(|(c, _)| Value::String(c.value.clone()))
                .collect(),
        ),
        Widget::Single { choices, sel, .. } => Value::String(
            sel.and_then(|i| choices.get(i)).map(|c| c.value.clone()).unwrap_or_default(),
        ),
        Widget::Toggle { on } => Value::Bool(*on),
        Widget::Input { buf, .. } => Value::String(buf.clone()),
    }
}

/// Run the wizard interactively. Returns true if completed, false if quit.
pub fn run_wizard_tui(session: &mut WizardSession, pal: Palette) -> anyhow::Result<bool> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let _g = TermGuard;
    let mut term: Terminal<CrosstermBackend<Stdout>> =
        Terminal::new(CrosstermBackend::new(io::stdout()))?;

    while !session.is_done() {
        let fields: Vec<Field> = session
            .fields()
            .into_iter()
            .map(|f| Field {
                id: f.id.clone(),
                field_type: f.field_type,
                prompt: f.prompt.clone(),
                default: f.default.clone(),
                required: f.required,
                source: f.source.clone(),
                options: f.options.clone(),
                condition: f.condition.clone(),
            })
            .collect();
        let mut widgets: Vec<Widget> =
            fields.iter().map(|f| init_widget(f, session)).collect();
        // focus targets: 0..fields = field i; fields = Back; fields+1 = Next
        let n = fields.len();
        let mut focus = 0usize;
        let mut err: Option<String> = None;
        let (title, desc) = session
            .current()
            .map(|p| (p.title.clone(), p.description.clone()))
            .unwrap_or_default();
        let (step, total) = session.progress();

        loop {
            term.draw(|fr| {
                let rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(4),
                        Constraint::Min(3),
                        Constraint::Length(3),
                    ])
                    .split(fr.area());

                let g = Gauge::default()
                    .block(Block::default().borders(Borders::ALL).title(format!(
                        " insmaller setup — {title}  (step {step}/{total}) "
                    )))
                    .gauge_style(Style::default().fg(pal.accent))
                    .ratio((step as f64 / total as f64).clamp(0.0, 1.0))
                    .label(desc.clone());
                fr.render_widget(g, rows[0]);

                let mut items: Vec<ListItem> = Vec::new();
                for (i, f) in fields.iter().enumerate() {
                    let focused = focus == i;
                    let head = format!(
                        "{} {}",
                        if focused { "▶" } else { " " },
                        f.prompt.as_deref().unwrap_or(&f.id)
                    );
                    items.push(ListItem::new(Span::styled(
                        head,
                        Style::default().add_modifier(Modifier::BOLD),
                    )));
                    match &widgets[i] {
                        Widget::Multi { choices, on, cur } => {
                            for (j, c) in choices.iter().enumerate() {
                                let mark = if on[j] { "[x]" } else { "[ ]" };
                                let p = if focused && *cur == j { ">" } else { " " };
                                items.push(ListItem::new(format!("   {p}{mark} {}", c.label)));
                            }
                        }
                        Widget::Single { choices, sel, cur } => {
                            for (j, c) in choices.iter().enumerate() {
                                let mark = if *sel == Some(j) { "(o)" } else { "( )" };
                                let p = if focused && *cur == j { ">" } else { " " };
                                items.push(ListItem::new(format!("   {p}{mark} {}", c.label)));
                            }
                        }
                        Widget::Toggle { on } => items.push(ListItem::new(format!(
                            "   [{}] (space toggles)",
                            if *on { "x" } else { " " }
                        ))),
                        Widget::Input { buf, secret } => {
                            let shown = if *secret {
                                "*".repeat(buf.chars().count())
                            } else {
                                buf.clone()
                            };
                            items.push(ListItem::new(format!(
                                "   {}{}",
                                shown,
                                if focused { "_" } else { "" }
                            )));
                        }
                    }
                }
                let body = List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(" fields "));
                fr.render_widget(body, rows[1]);

                let btn = |label: &str, idx: usize, enabled: bool| {
                    let st = if !enabled {
                        Style::default().fg(pal.muted)
                    } else if focus == idx {
                        Style::default().fg(pal.accent_fg).bg(pal.accent)
                    } else {
                        Style::default().fg(pal.accent)
                    };
                    Span::styled(format!(" {label} "), st)
                };
                let foot = Line::from(vec![
                    btn("◄ Back", n, session.can_back()),
                    Span::raw("  "),
                    btn("Next ►", n + 1, true),
                    Span::raw("   "),
                    Span::styled(
                        err.clone().unwrap_or_else(|| {
                            "Tab/←→ focus · ↑↓ move · Space toggle · Enter next · Esc back · q quit".into()
                        }),
                        Style::default().fg(if err.is_some() { pal.error } else { pal.muted }),
                    ),
                ]);
                fr.render_widget(
                    Paragraph::new(foot).block(Block::default().borders(Borders::ALL)),
                    rows[2],
                );
            })?;

            let Event::Key(k) = event::read()? else { continue };
            if k.kind != KeyEventKind::Press {
                continue;
            }
            let editing = matches!(widgets.get(focus), Some(Widget::Input { .. }));
            // quit
            if (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
                || (k.code == KeyCode::Char('q') && !editing)
            {
                return Ok(false);
            }

            let commit = |ws: &[Widget], fs: &[Field]| -> Map<String, Value> {
                let mut m = Map::new();
                for (w, f) in ws.iter().zip(fs) {
                    m.insert(f.id.clone(), widget_value(w));
                }
                m
            };

            match k.code {
                KeyCode::Tab | KeyCode::Right if !editing => focus = (focus + 1) % (n + 2),
                KeyCode::BackTab | KeyCode::Left if !editing => {
                    focus = (focus + n + 1) % (n + 2)
                }
                KeyCode::Esc => {
                    let m = commit(&widgets, &fields);
                    session.store(m);
                    if session.back() {
                        break;
                    }
                }
                KeyCode::Up if focus < n => {
                    if let Some(c) = match &mut widgets[focus] {
                        Widget::Multi { cur, .. } | Widget::Single { cur, .. } => Some(cur),
                        _ => None,
                    } {
                        *c = c.saturating_sub(1);
                    }
                }
                KeyCode::Down if focus < n => {
                    let len = match &widgets[focus] {
                        Widget::Multi { choices, .. } | Widget::Single { choices, .. } => {
                            choices.len()
                        }
                        _ => 0,
                    };
                    if let Some(c) = match &mut widgets[focus] {
                        Widget::Multi { cur, .. } | Widget::Single { cur, .. } => Some(cur),
                        _ => None,
                    } {
                        if *c + 1 < len {
                            *c += 1;
                        }
                    }
                }
                KeyCode::Char(' ') if focus < n => match &mut widgets[focus] {
                    Widget::Multi { on, cur, .. } => on[*cur] = !on[*cur],
                    Widget::Single { sel, cur, .. } => *sel = Some(*cur),
                    Widget::Toggle { on } => *on = !*on,
                    Widget::Input { buf, .. } => buf.push(' '),
                },
                KeyCode::Char(ch) if editing => {
                    if let Widget::Input { buf, .. } = &mut widgets[focus] {
                        buf.push(ch);
                    }
                }
                KeyCode::Backspace if editing => {
                    if let Widget::Input { buf, .. } = &mut widgets[focus] {
                        buf.pop();
                    }
                }
                KeyCode::Enter => {
                    if focus == n {
                        // Back button
                        let m = commit(&widgets, &fields);
                        session.store(m);
                        if session.back() {
                            break;
                        }
                    } else {
                        // Next (or any field) → submit page
                        let m = commit(&widgets, &fields);
                        match session.submit(m) {
                            Ok(()) => break,
                            Err(e) => err = Some(format!("{e}")),
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(true)
}

/// indicatif spinner reporter for the install phase.
pub struct BarReporter {
    bar: ProgressBar,
}
impl BarReporter {
    // indicatif's template color is a static token (no arbitrary RGB), so the
    // spinner only honors the colored/mono distinction, not custom hex.
    pub fn new(pal: Palette) -> Self {
        let bar = ProgressBar::new_spinner();
        let tmpl = if pal.colored() {
            "{spinner:.cyan} {wide_msg}"
        } else {
            "{spinner} {wide_msg}"
        };
        bar.set_style(
            ProgressStyle::with_template(tmpl)
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        Self { bar }
    }
    pub fn finish(&self) {
        self.bar.finish_and_clear();
    }
}
impl Reporter for BarReporter {
    fn step_start(&self, key: &str, step_type: &str) {
        self.bar.set_message(format!("{key} · {step_type}"));
    }
    fn step_end(&self, key: &str, step_type: &str, ok: bool) {
        if !ok {
            self.bar
                .println(format!("  ✗ {key} · {step_type}"));
        }
    }
    fn log(&self, msg: &str) {
        self.bar.println(msg);
    }
}
