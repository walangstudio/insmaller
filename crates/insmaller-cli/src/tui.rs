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
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph},
    Terminal,
};
use crate::theme::Palette;
use serde_json::{Map, Value};
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};

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
    /// A filesystem path. Editable as text; `Ctrl+B` opens an interactive
    /// directory/file browser (`picker = Some`).
    Path { buf: String, picker: Option<Picker> },
}

/// One row in the file browser.
struct Entry {
    name: String,
    is_dir: bool,
}

/// Interactive directory/file browser overlaid on a `Path` field.
struct Picker {
    cwd: PathBuf,
    entries: Vec<Entry>,
    /// false ⇒ `cwd` could not be read (permissions, gone). `entries` then
    /// holds only `..`; the modal shows the state so the user isn't left
    /// staring at a silently-empty list.
    readable: bool,
    cursor: usize,
}

/// Directory listing for the browser: `..` first (unless at a root), then
/// directories before files, each group case-insensitively sorted. Returns
/// `(entries, readable)` — `readable` is false when the dir can't be opened, so
/// callers can distinguish "empty" from "denied". Pure given the filesystem —
/// unit-testable against a tempdir.
fn list_dir(p: &Path) -> (Vec<Entry>, bool) {
    let mut entries: Vec<Entry> = Vec::new();
    if p.parent().is_some() {
        entries.push(Entry { name: "..".into(), is_dir: true });
    }
    match std::fs::read_dir(p) {
        Ok(rd) => {
            let mut items: Vec<Entry> = rd
                .flatten()
                .map(|d| Entry {
                    name: d.file_name().to_string_lossy().into_owned(),
                    is_dir: d.file_type().map(|t| t.is_dir()).unwrap_or(false),
                })
                .collect();
            items.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            entries.extend(items);
            (entries, true)
        }
        Err(_) => (entries, false),
    }
}

impl Picker {
    /// Seed the browser at `buf`'s directory (or its parent if `buf` names a
    /// file), falling back to the home dir.
    fn open(buf: &str) -> Picker {
        let mut p = Picker {
            cwd: PathBuf::new(),
            entries: Vec::new(),
            readable: true,
            cursor: 0,
        };
        p.set_dir(Self::seed_dir(buf));
        p
    }

    /// Move to `dir`: relist, reset the cursor, record readability.
    fn set_dir(&mut self, dir: PathBuf) {
        let (entries, readable) = list_dir(&dir);
        self.cwd = dir;
        self.entries = entries;
        self.readable = readable;
        self.cursor = 0;
    }

    fn seed_dir(buf: &str) -> PathBuf {
        let home = || dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        if buf.is_empty() {
            return home();
        }
        let p = PathBuf::from(buf);
        if p.is_dir() {
            return p;
        }
        match p.parent() {
            Some(parent) if parent.is_dir() => parent.to_path_buf(),
            _ => home(),
        }
    }

    fn up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn down(&mut self) {
        if self.cursor + 1 < self.entries.len() {
            self.cursor += 1;
        }
    }

    fn ascend(&mut self) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.set_dir(parent);
        }
    }

    /// Enter/→ on the cursor: descend into a directory (or `..`) and return
    /// `None`; on a file, return its full path (caller closes the picker).
    fn activate(&mut self) -> Option<String> {
        let entry = self.entries.get(self.cursor)?;
        if entry.name == ".." {
            self.ascend();
            return None;
        }
        let target = self.cwd.join(&entry.name);
        if entry.is_dir {
            self.set_dir(target);
            None
        } else {
            Some(target.to_string_lossy().into_owned())
        }
    }

    /// The current directory itself, as the selected value.
    fn select_cwd(&self) -> String {
        self.cwd.to_string_lossy().into_owned()
    }
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
        FieldType::Path => Widget::Path {
            buf: match prior {
                Some(Value::String(s)) => s,
                _ => f.default.clone().unwrap_or_default(),
            },
            picker: None,
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
        Widget::Path { buf, .. } => Value::String(buf.clone()),
    }
}

/// Vertical (↑/↓) navigation. Within a select's choices while there's room to
/// move; otherwise fall through to field navigation. `len` is the focused
/// select's choice count (0 for Input/Toggle/edge-less widgets, which always
/// move focus). Returns `(new_cur, new_focus)`; `new_cur` is only meaningful
/// for selects. Focus is clamped to `0..=n+1` (fields, then Back, then Next).
fn vert_nav(cur: usize, len: usize, down: bool, focus: usize, n: usize) -> (usize, usize) {
    if down {
        if len > 0 && cur + 1 < len {
            (cur + 1, focus)
        } else {
            (cur, (focus + 1).min(n + 1))
        }
    } else if len > 0 && cur > 0 {
        (cur - 1, focus)
    } else {
        (cur, focus.saturating_sub(1))
    }
}

/// A rectangle centered in `area`, `percent_x` × `percent_y` of its size.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(v[1])[1]
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
                        Widget::Path { buf, .. } => {
                            items.push(ListItem::new(format!(
                                "   {}{}",
                                buf,
                                if focused { "_   [Ctrl+B browse]" } else { "" }
                            )));
                        }
                    }
                }
                let body = List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(" fields "));
                fr.render_widget(body, rows[1]);

                // Path browser overlay (captures all keys while open).
                if let Some(Widget::Path { picker: Some(p), .. }) = widgets.get(focus) {
                    let area = centered_rect(70, 70, fr.area());
                    let rows_p: Vec<ListItem> = p
                        .entries
                        .iter()
                        .map(|e| {
                            let name = if e.is_dir && e.name != ".." {
                                format!("{}/", e.name)
                            } else {
                                e.name.clone()
                            };
                            ListItem::new(name)
                        })
                        .collect();
                    let state = if p.readable { "" } else { "  [unreadable]" };
                    let title = format!(
                        " {}{}  (↑↓ move · ↵ open · ← up · s select dir · Esc cancel) ",
                        p.cwd.display(),
                        state
                    );
                    let list = List::new(rows_p)
                        .block(Block::default().borders(Borders::ALL).title(title))
                        .highlight_style(Style::default().fg(pal.accent_fg).bg(pal.accent))
                        .highlight_symbol("> ");
                    let mut st = ListState::default();
                    st.select(Some(p.cursor));
                    fr.render_widget(Clear, area);
                    fr.render_stateful_widget(list, area, &mut st);
                }

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
                            "Tab/←→ focus · ↑↓ move within/between fields · Space toggle · Enter next · Esc back · q quit".into()
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

            // Ctrl+C always quits, even with the browser open.
            if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                return Ok(false);
            }

            // An open path browser owns every key until it closes.
            if matches!(widgets.get(focus), Some(Widget::Path { picker: Some(_), .. })) {
                if let Some(Widget::Path { buf, picker }) = widgets.get_mut(focus) {
                    let p = picker.as_mut().expect("picker is Some");
                    match k.code {
                        KeyCode::Up => p.up(),
                        KeyCode::Down => p.down(),
                        KeyCode::Left | KeyCode::Backspace => p.ascend(),
                        KeyCode::Enter | KeyCode::Right => {
                            if let Some(path) = p.activate() {
                                *buf = path;
                                *picker = None;
                            }
                        }
                        KeyCode::Char('s') => {
                            *buf = p.select_cwd();
                            *picker = None;
                        }
                        KeyCode::Esc => *picker = None,
                        _ => {}
                    }
                }
                continue;
            }

            // Ctrl+B opens the browser on a focused path field.
            if k.code == KeyCode::Char('b') && k.modifiers.contains(KeyModifiers::CONTROL) {
                if let Some(Widget::Path { buf, picker }) = widgets.get_mut(focus) {
                    *picker = Some(Picker::open(buf));
                }
                continue;
            }

            let editing = matches!(
                widgets.get(focus),
                Some(Widget::Input { .. }) | Some(Widget::Path { .. })
            );
            // quit
            if k.code == KeyCode::Char('q') && !editing {
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
                KeyCode::Up | KeyCode::Down if focus < n => {
                    let down = k.code == KeyCode::Down;
                    let (cur, len) = match &widgets[focus] {
                        Widget::Multi { choices, cur, .. }
                        | Widget::Single { choices, cur, .. } => (*cur, choices.len()),
                        _ => (0, 0),
                    };
                    let (new_cur, new_focus) = vert_nav(cur, len, down, focus, n);
                    if let Widget::Multi { cur, .. } | Widget::Single { cur, .. } =
                        &mut widgets[focus]
                    {
                        *cur = new_cur;
                    }
                    focus = new_focus;
                }
                KeyCode::Char(' ') if focus < n => match &mut widgets[focus] {
                    Widget::Multi { on, cur, .. } => on[*cur] = !on[*cur],
                    Widget::Single { sel, cur, .. } => *sel = Some(*cur),
                    Widget::Toggle { on } => *on = !*on,
                    Widget::Input { buf, .. } | Widget::Path { buf, .. } => buf.push(' '),
                },
                KeyCode::Char(ch) if editing => {
                    if let Widget::Input { buf, .. } | Widget::Path { buf, .. } =
                        &mut widgets[focus]
                    {
                        buf.push(ch);
                    }
                }
                KeyCode::Backspace if editing => {
                    if let Widget::Input { buf, .. } | Widget::Path { buf, .. } =
                        &mut widgets[focus]
                    {
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

#[cfg(test)]
mod tests {
    use super::{list_dir, vert_nav, Picker};

    // 2 fields (n=2): focus 0,1 = fields; 2 = Back; 3 = Next.
    #[test]
    fn down_within_select_then_to_next_field() {
        // field 0 is a 3-choice select at cursor 0
        assert_eq!(vert_nav(0, 3, true, 0, 2), (1, 0));
        assert_eq!(vert_nav(1, 3, true, 0, 2), (2, 0));
        // at the last choice, Down advances focus to field 1
        assert_eq!(vert_nav(2, 3, true, 0, 2), (2, 1));
    }

    #[test]
    fn up_within_select_then_to_prev_field() {
        // field 1 select at cursor 2 → cursor 1 → cursor 0 → prev field
        assert_eq!(vert_nav(2, 3, false, 1, 2), (1, 1));
        assert_eq!(vert_nav(1, 3, false, 1, 2), (0, 1));
        assert_eq!(vert_nav(0, 3, false, 1, 2), (0, 0));
    }

    #[test]
    fn fieldless_widget_moves_focus_both_ways() {
        // len 0 (Input/Toggle): arrows move focus immediately
        assert_eq!(vert_nav(0, 0, true, 0, 2), (0, 1));
        assert_eq!(vert_nav(0, 0, false, 1, 2), (0, 0));
    }

    #[test]
    fn focus_clamps_at_edges() {
        // Down past the last field lands on Back (n) then Next (n+1), no further
        assert_eq!(vert_nav(0, 0, true, 2, 2), (0, 3));
        assert_eq!(vert_nav(0, 0, true, 3, 2), (0, 3));
        // Up from field 0 stays at 0
        assert_eq!(vert_nav(0, 0, false, 0, 2), (0, 0));
    }

    #[test]
    fn list_dir_dotdot_first_then_dirs_before_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("zdir")).unwrap();
        std::fs::write(dir.path().join("afile.txt"), b"x").unwrap();
        let (entries, readable) = list_dir(dir.path());
        assert!(readable);
        assert_eq!(entries[0].name, "..");
        assert!(entries[0].is_dir);
        // directory sorts before the file despite "zdir" > "afile"
        assert_eq!(entries[1].name, "zdir");
        assert!(entries[1].is_dir);
        assert_eq!(entries[2].name, "afile.txt");
        assert!(!entries[2].is_dir);
    }

    #[test]
    fn list_dir_reports_unreadable() {
        // A path that is not a directory cannot be listed → readable=false,
        // and only the synthetic ".." entry is present.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not_a_dir.txt");
        std::fs::write(&file, b"x").unwrap();
        let (entries, readable) = list_dir(&file);
        assert!(!readable);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "..");
    }

    #[test]
    fn picker_descends_ascends_and_selects_file() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("f.txt"), b"x").unwrap();

        let mut p = Picker::open(&dir.path().to_string_lossy());
        // [.., sub]; move onto "sub" and descend
        p.down();
        assert_eq!(p.entries[p.cursor].name, "sub");
        assert_eq!(p.activate(), None);
        assert_eq!(p.cwd, sub);

        // now [.., f.txt]; selecting the file returns its full path
        p.down();
        assert_eq!(p.entries[p.cursor].name, "f.txt");
        let got = p.activate().expect("file selection returns a path");
        assert_eq!(std::path::PathBuf::from(got), sub.join("f.txt"));

        // activating ".." ascends back to the parent
        let mut q = Picker::open(&sub.to_string_lossy());
        assert_eq!(q.entries[0].name, "..");
        assert_eq!(q.activate(), None);
        assert_eq!(q.cwd, dir.path());
    }
}
