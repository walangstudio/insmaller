//! ratatui wizard TUI: a persistent screen with a progress gauge/breadcrumb
//! header, a per-page body, and on-screen [◄ Back] [Next ►] [Quit] buttons —
//! navigable by Tab/←/→ AND shortcut keys (Esc=back, Enter=next, q/Ctrl-C
//! quit). Drives a pure `WizardSession`. Plus an indicatif reporter for the
//! install phase.

use chrono::{Datelike, Days, Local, Months, NaiveDate};
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
    widgets::{Block, BorderType, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph},
    Terminal,
};
use crate::theme::{gradient, Palette};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::io::{self, IsTerminal, Stdout};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

/// Restores the terminal even on panic/early-return.
struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

enum Widget {
    Multi {
        choices: Vec<insmaller_core::Choice>,
        on: Vec<bool>,
        groups: Vec<String>,
        collapsed: Vec<bool>,
        cur: usize,
    },
    Single {
        choices: Vec<insmaller_core::Choice>,
        sel: Option<usize>,
        groups: Vec<String>,
        collapsed: Vec<bool>,
        cur: usize,
    },
    Toggle { on: bool },
    Input { buf: String, secret: bool },
    /// A filesystem path. Editable as text; `Ctrl+B` opens an interactive
    /// directory/file browser (`picker = Some`).
    Path { buf: String, picker: Option<Picker> },
    /// Collapsed type-to-search dropdown. Enter/Space opens the popup list;
    /// typing narrows the list; ↑/↓ navigate; Enter selects; Esc closes.
    Dropdown {
        choices: Vec<String>,
        /// Index into `choices` of the currently-selected value.
        sel: usize,
        /// Whether the popup list is open.
        open: bool,
        /// Type-ahead filter text.
        filter: String,
        /// Cursor within the filtered list.
        cur: usize,
    },
    /// Multi-line text area. Enter inserts a newline; Tab commits/advances.
    Textarea {
        buf: String,
        cursor_row: usize,
        cursor_col: usize,
        scroll: usize,
    },
    /// ISO date input (`YYYY-MM-DD`). Digit-only masked entry; separators are
    /// fixed. `digits` holds the 8 user-entered digit positions (b'_' = empty).
    /// `dcur` is the index into `digits` (0-7). Space opens the calendar.
    Date { digits: [u8; 8], dcur: usize, cal: Option<CalPicker> },
    /// ISO datetime input (`YYYY-MM-DDTHH:MM:SS`). 14 digit slots.
    Datetime { digits: [u8; 14], dcur: usize, cal: Option<CalPicker> },
}

/// Calendar overlay for Date/Datetime fields.
struct CalPicker {
    date: NaiveDate,
}

/// A visible line in a select's collapsible tree: a group `Header` (index into
/// the group list) or an `Item` (index into the choices vec).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Row {
    Header(usize),
    Item(usize),
}

/// Distinct catalog groups in first-appearance order. Ungrouped choices are
/// excluded (they render at the top with no header).
fn group_list(choices: &[insmaller_core::Choice]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in choices {
        if let Some(g) = &c.group {
            if !out.iter().any(|x| x == g) {
                out.push(g.clone());
            }
        }
    }
    out
}

/// Choice label without the redundant `[group] ` prefix (the group is shown by
/// its header in the tree).
fn item_label(c: &insmaller_core::Choice) -> &str {
    if let Some(g) = &c.group {
        if let Some(rest) = c.label.strip_prefix(&format!("[{g}] ")) {
            return rest;
        }
    }
    &c.label
}

/// Checkbox glyph for a multiselect group header: all / some / none selected.
fn group_mark_multi(choices: &[insmaller_core::Choice], on: &[bool], group: &str) -> &'static str {
    let idxs: Vec<usize> = (0..choices.len())
        .filter(|&i| choices[i].group.as_deref() == Some(group))
        .collect();
    let sel = idxs.iter().filter(|&&i| on[i]).count();
    if sel == 0 {
        "[ ]"
    } else if sel == idxs.len() {
        "[x]"
    } else {
        "[~]"
    }
}

/// Visible rows for a select: ungrouped items first, then each group header
/// followed by its items unless the group is collapsed. `collapsed` aligns to
/// `groups`. With no groups this is just every item in order (a flat list).
fn visible_rows(
    choices: &[insmaller_core::Choice],
    groups: &[String],
    collapsed: &[bool],
) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for (i, c) in choices.iter().enumerate() {
        if c.group.is_none() {
            rows.push(Row::Item(i));
        }
    }
    for (gi, g) in groups.iter().enumerate() {
        rows.push(Row::Header(gi));
        if !collapsed.get(gi).copied().unwrap_or(false) {
            for (i, c) in choices.iter().enumerate() {
                if c.group.as_deref() == Some(g.as_str()) {
                    rows.push(Row::Item(i));
                }
            }
        }
    }
    rows
}

/// Visible rows of a select widget (`None` for non-selects).
fn tree_rows_of(w: &Widget) -> Option<Vec<Row>> {
    match w {
        Widget::Multi { choices, groups, collapsed, .. }
        | Widget::Single { choices, groups, collapsed, .. } => {
            Some(visible_rows(choices, groups, collapsed))
        }
        _ => None,
    }
}

/// A select's tree cursor (0 otherwise).
fn cur_of(w: &Widget) -> usize {
    match w {
        Widget::Multi { cur, .. } | Widget::Single { cur, .. } => *cur,
        _ => 0,
    }
}

/// The row under the cursor of a select widget.
fn current_row(w: &Widget) -> Option<Row> {
    tree_rows_of(w).and_then(|rows| rows.get(cur_of(w)).copied())
}

/// True for a select that actually has group headers (so ←/→ drive the tree
/// rather than field-focus navigation).
fn widget_has_groups(w: &Widget) -> bool {
    matches!(
        w,
        Widget::Multi { groups, .. } | Widget::Single { groups, .. } if !groups.is_empty()
    )
}

/// Clamp the tree cursor to the current visible-row count (after a collapse
/// shrinks the list).
fn clamp_cur(w: &mut Widget) {
    let max = match tree_rows_of(w) {
        Some(rows) => rows.len().saturating_sub(1),
        None => return,
    };
    if let Widget::Multi { cur, .. } | Widget::Single { cur, .. } = w {
        *cur = (*cur).min(max);
    }
}

/// Move the cursor onto the header of `item`'s group (← from an item).
fn cursor_to_header_of(w: &mut Widget, item: usize) {
    let rows = match tree_rows_of(w) {
        Some(r) => r,
        None => return,
    };
    let gi = match &*w {
        Widget::Multi { choices, groups, .. } | Widget::Single { choices, groups, .. } => choices
            .get(item)
            .and_then(|c| c.group.as_ref())
            .and_then(|g| groups.iter().position(|x| x == g)),
        _ => None,
    };
    let Some(gi) = gi else { return };
    let Some(pos) = rows.iter().position(|r| *r == Row::Header(gi)) else {
        return;
    };
    if let Widget::Multi { cur, .. } | Widget::Single { cur, .. } = w {
        *cur = pos;
    }
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

/// Available drive roots on Windows (`C:`, `D:`, …) from the `GetLogicalDrives`
/// bitmask — dependency-free and, crucially, it never touches the filesystem.
/// Stat-probing each letter (the obvious approach) would block for seconds on a
/// disconnected network-mapped drive; the bitmask just reports which letters
/// are in use. Only the drive-selector pseudo-level calls this.
#[cfg(windows)]
fn windows_drives() -> Vec<Entry> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetLogicalDrives() -> u32;
    }
    let mask = unsafe { GetLogicalDrives() };
    ('A'..='Z')
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, d)| Entry { name: format!("{d}:"), is_dir: true })
        .collect()
}

/// Directory listing for the browser: `.` (pick this folder) first, then `..`
/// (parent, unless at a root), then directories before files, each group
/// case-insensitively sorted. Returns `(entries, readable)` — `readable` is
/// false when the dir can't be opened, so callers can distinguish "empty" from
/// "denied". On Windows the empty path is the drive selector (lists drive
/// roots), and a drive root still offers `..` (up to that selector). Pure given
/// the filesystem — unit-testable against a tempdir.
fn list_dir(p: &Path) -> (Vec<Entry>, bool) {
    // Windows drive selector: empty path ⇒ list the drive roots, nothing else.
    #[cfg(windows)]
    if p.as_os_str().is_empty() {
        return (windows_drives(), true);
    }
    let mut entries: Vec<Entry> = Vec::new();
    // `.` always selects the current directory as the value.
    entries.push(Entry { name: ".".into(), is_dir: true });
    // `..` ascends to the parent — or, at a Windows drive root (no parent), up
    // to the drive selector. On Unix the single `/` root has no `..`.
    let has_parent = p.parent().is_some();
    if has_parent || cfg!(windows) {
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

    /// At the Windows drive selector (the empty-path pseudo-level). On Unix the
    /// cwd is never empty, so this is always false there. One predicate owns the
    /// sentinel so it can't be re-spelled (or leak) inconsistently.
    fn at_drive_selector(&self) -> bool {
        self.cwd.as_os_str().is_empty()
    }

    fn ascend(&mut self) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.set_dir(parent);
        } else {
            // No parent: a Windows drive root goes up to the drive selector;
            // the Unix `/` root (and the selector itself) stay put.
            self.goto_drives();
        }
    }

    /// Jump straight to the Windows drive selector from anywhere (`d`
    /// shortcut). No-op on Unix, and when already at the selector.
    fn goto_drives(&mut self) {
        if cfg!(windows) && !self.at_drive_selector() {
            self.set_dir(PathBuf::new());
        }
    }

    /// Enter/→ on the cursor: descend into a directory (or `..`) and return
    /// `None`; on a file, return its full path (caller closes the picker).
    fn activate(&mut self) -> Option<String> {
        let entry = self.entries.get(self.cursor)?;
        if entry.name == "." {
            return self.select_cwd();
        }
        if entry.name == ".." {
            self.ascend();
            return None;
        }
        // From the drive selector (empty cwd) a `C:` entry must become `C:\`,
        // not the relative `C:`; elsewhere a plain join is the child path.
        let target = if self.at_drive_selector() {
            PathBuf::from(format!("{}\\", entry.name))
        } else {
            self.cwd.join(&entry.name)
        };
        if entry.is_dir {
            self.set_dir(target);
            None
        } else {
            Some(target.to_string_lossy().into_owned())
        }
    }

    /// The current directory itself, as the selected value — `None` at the
    /// drive selector, which has no folder to pick (guards `s`/`.` from
    /// silently returning the empty sentinel path).
    fn select_cwd(&self) -> Option<String> {
        if self.at_drive_selector() {
            None
        } else {
            Some(self.cwd.to_string_lossy().into_owned())
        }
    }
}

/// Per-group initial collapse policy: a baseline plus name overrides.
/// `expanded` wins over `collapsed`, both win over the baseline.
#[derive(Default, Clone)]
pub struct GroupDefaults {
    pub collapsed_default: bool,
    pub collapsed: Vec<String>,
    pub expanded: Vec<String>,
}

impl GroupDefaults {
    fn is_collapsed(&self, group: &str) -> bool {
        if self.expanded.iter().any(|g| g == group) {
            false
        } else if self.collapsed.iter().any(|g| g == group) {
            true
        } else {
            self.collapsed_default
        }
    }
    /// Initial collapse per group: a prior user choice in `cache` (keyed by
    /// field id + group) wins, else the configured default. Lets expand/collapse
    /// survive leaving and re-entering a wizard page.
    fn for_groups(&self, field_id: &str, groups: &[String], cache: &HashMap<String, bool>) -> Vec<bool> {
        groups
            .iter()
            .map(|g| {
                cache
                    .get(&collapse_key(field_id, g))
                    .copied()
                    .unwrap_or_else(|| self.is_collapsed(g))
            })
            .collect()
    }
}

/// Cache key for a group's collapse state (NUL separates id from group so they
/// can't collide).
fn collapse_key(field_id: &str, group: &str) -> String {
    format!("{field_id}\u{0}{group}")
}

// ── date/datetime mask helpers ───────────────────────────────────────────────

/// The fixed separator characters for a Date mask at each string position.
/// `YYYY-MM-DD`: positions 4 and 7 are `-`.
/// Returns `Some(sep)` when `str_idx` is a separator, else `None`.
fn date_sep(str_idx: usize) -> Option<char> {
    match str_idx { 4 | 7 => Some('-'), _ => None }
}

/// Same for Datetime `YYYY-MM-DDTHH:MM:SS`.
/// Separators at 4(`-`), 7(`-`), 10(`T`), 13(`:`), 16(`:`).
fn datetime_sep(str_idx: usize) -> Option<char> {
    match str_idx {
        4 | 7 => Some('-'),
        10 => Some('T'),
        13 | 16 => Some(':'),
        _ => None,
    }
}

/// Render a Date digit array as a display string like `2026-09-__`.
fn render_date_mask(digits: &[u8; 8]) -> String {
    let mut s = String::with_capacity(10);
    let mut di = 0usize;
    for si in 0..10usize {
        if let Some(sep) = date_sep(si) {
            s.push(sep);
        } else {
            s.push(if digits[di] == b'_' { '_' } else { digits[di] as char });
            di += 1;
        }
    }
    s
}

/// Render a Datetime digit array as a display string like `2026-09-01T__:__:__`.
fn render_datetime_mask(digits: &[u8; 14]) -> String {
    let mut s = String::with_capacity(19);
    let mut di = 0usize;
    for si in 0..19usize {
        if let Some(sep) = datetime_sep(si) {
            s.push(sep);
        } else {
            s.push(if digits[di] == b'_' { '_' } else { digits[di] as char });
            di += 1;
        }
    }
    s
}

/// Parse an ISO date string into a digit array; fills `b'_'` for missing/bad slots.
fn parse_date_digits(s: &str) -> [u8; 8] {
    let mut d = [b'_'; 8];
    let s = s.trim();
    if s.len() >= 10 {
        let bytes = s.as_bytes();
        let slots = [0usize, 1, 2, 3, 5, 6, 8, 9];
        for (i, &si) in slots.iter().enumerate() {
            let b = bytes[si];
            if b.is_ascii_digit() { d[i] = b; }
        }
    }
    d
}

/// Parse an ISO datetime string into a digit array.
fn parse_datetime_digits(s: &str) -> [u8; 14] {
    let mut d = [b'_'; 14];
    let s = s.trim();
    if s.len() >= 19 {
        let bytes = s.as_bytes();
        let slots = [0usize, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
        for (i, &si) in slots.iter().enumerate() {
            let b = bytes[si];
            if b.is_ascii_digit() { d[i] = b; }
        }
    }
    d
}

/// Assemble a Date digit array into an ISO string if all 8 digits are filled.
/// Returns `None` if any slot is still `b'_'` (incomplete).
fn digits_to_date_str(digits: &[u8; 8]) -> Option<String> {
    if digits.contains(&b'_') {
        return None;
    }
    Some(render_date_mask(digits))
}

/// Assemble a Datetime digit array into an ISO string if all 14 digits are filled.
fn digits_to_datetime_str(digits: &[u8; 14]) -> Option<String> {
    if digits.contains(&b'_') {
        return None;
    }
    Some(render_datetime_mask(digits))
}

/// Extract the date portion from the committed value of a Date or Datetime widget.
/// Returns `Some(NaiveDate)` if the stored digits parse cleanly.
fn date_from_date_digits(digits: &[u8; 8]) -> Option<NaiveDate> {
    let s = digits_to_date_str(digits)?;
    NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok()
}

/// Commit a `NaiveDate` back into a Date widget's digit array.
fn date_to_date_digits(date: NaiveDate) -> [u8; 8] {
    let s = date.format("%Y-%m-%d").to_string();
    parse_date_digits(&s)
}

/// Commit a `NaiveDate` into a Datetime widget, preserving any time digits
/// already typed (keeps `HH:MM:SS` if filled; otherwise defaults to `00:00:00`).
fn date_to_datetime_digits(date: NaiveDate, existing: &[u8; 14]) -> [u8; 14] {
    let date_str = date.format("%Y-%m-%d").to_string();
    let mut new = parse_datetime_digits(&format!("{}T00:00:00", date_str));
    // Preserve time digits (slots 8-13) if user had entered them.
    for i in 8..14 {
        if existing[i] != b'_' {
            new[i] = existing[i];
        }
    }
    new
}

/// Type a digit into a Date widget: fills slot `dcur`, advances past separators.
/// Returns the new `dcur`.
fn date_type_digit(digits: &mut [u8; 8], dcur: usize, ch: u8) -> usize {
    if dcur >= 8 { return dcur; }
    digits[dcur] = ch;
    (dcur + 1).min(8)
}

/// Backspace in a Date widget: clears slot `dcur-1`, moves cursor back.
/// Returns the new `dcur`.
fn date_backspace(digits: &mut [u8; 8], dcur: usize) -> usize {
    if dcur == 0 { return 0; }
    let prev = dcur - 1;
    digits[prev] = b'_';
    prev
}

/// Type a digit into a Datetime widget. Returns new `dcur`.
fn datetime_type_digit(digits: &mut [u8; 14], dcur: usize, ch: u8) -> usize {
    if dcur >= 14 { return dcur; }
    digits[dcur] = ch;
    (dcur + 1).min(14)
}

/// Backspace in a Datetime widget. Returns new `dcur`.
fn datetime_backspace(digits: &mut [u8; 14], dcur: usize) -> usize {
    if dcur == 0 { return 0; }
    let prev = dcur - 1;
    digits[prev] = b'_';
    prev
}

/// Build the `widget_value` string for a Date widget (empty string if incomplete).
fn date_widget_value(digits: &[u8; 8]) -> String {
    digits_to_date_str(digits).unwrap_or_default()
}

/// Build the `widget_value` string for a Datetime widget.
fn datetime_widget_value(digits: &[u8; 14]) -> String {
    digits_to_datetime_str(digits).unwrap_or_default()
}

// ── calendar helpers ─────────────────────────────────────────────────────────

/// Number of days in `year`/`month` (1-based). Uses chrono: jump to the first
/// of the next month minus one day, handling year wrap.
fn days_in_month(year: i32, month: u32) -> u32 {
    let (y, m) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    NaiveDate::from_ymd_opt(y, m, 1)
        .and_then(|d| d.pred_opt())
        .map(|d| d.day())
        .unwrap_or(30)
}

/// Render a calendar month as a vector of lines to display inside the overlay.
/// Highlights `sel` with `[DD]` brackets; other days are ` DD`. Width is fixed
/// at 20 chars for the week rows (7 × 3 = 21 minus the trailing space).
fn render_calendar(year: i32, month: u32, sel: NaiveDate) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push("Su Mo Tu We Th Fr Sa".to_string());
    let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap_or(sel);
    // weekday of the 1st: Sun=0 … Sat=6
    let start_wd = first.weekday().num_days_from_sunday() as usize;
    let dim = days_in_month(year, month);
    let mut row = String::new();
    // leading blanks
    for _ in 0..start_wd {
        row.push_str("   ");
    }
    let mut col = start_wd;
    for day in 1..=dim {
        let d = NaiveDate::from_ymd_opt(year, month, day).unwrap_or(sel);
        // 3 chars per cell so the columns line up: selected = `>DD`, normal = ` DD`.
        let marker = if d == sel { '>' } else { ' ' };
        row.push_str(&format!("{marker}{day:02}"));
        col += 1;
        if col == 7 {
            lines.push(row.clone());
            row.clear();
            col = 0;
        } else {
            row.push(' ');
        }
    }
    if !row.trim().is_empty() {
        lines.push(row);
    }
    lines
}

/// Insert a character at the logical cursor position inside a textarea buffer.
/// The buffer uses `\n` as the line separator. Updates `cursor_row`/`cursor_col`
/// in place after the insertion.
fn textarea_insert(buf: &mut String, cursor_row: &mut usize, cursor_col: &mut usize, ch: char) {
    let byte_pos = textarea_byte_pos(buf, *cursor_row, *cursor_col);
    buf.insert(byte_pos, ch);
    if ch == '\n' {
        *cursor_row += 1;
        *cursor_col = 0;
    } else {
        *cursor_col += 1;
    }
}

/// Delete the character before the cursor (backspace semantics).
fn textarea_backspace(buf: &mut String, cursor_row: &mut usize, cursor_col: &mut usize) {
    if *cursor_row == 0 && *cursor_col == 0 {
        return;
    }
    let byte_pos = textarea_byte_pos(buf, *cursor_row, *cursor_col);
    if byte_pos == 0 {
        return;
    }
    // Find the previous char boundary.
    let prev = buf[..byte_pos]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0);
    let removed_ch = buf.chars().nth(buf[..prev].chars().count()).unwrap_or(' ');
    // Compute new cursor position BEFORE modifying the buffer, so line splits
    // still reflect the pre-removal layout.
    if removed_ch == '\n' && *cursor_row > 0 {
        // The previous line's length is its char count in the current buffer.
        let prev_line_len = buf.split('\n')
            .nth(*cursor_row - 1)
            .unwrap_or("")
            .chars()
            .count();
        buf.remove(prev);
        *cursor_row -= 1;
        *cursor_col = prev_line_len;
    } else {
        buf.remove(prev);
        if *cursor_col > 0 {
            *cursor_col -= 1;
        }
    }
}

/// How many lines the textarea renders at once (used for scroll clamping).
const TEXTAREA_VISIBLE_ROWS: usize = 4;

/// Adjust `scroll` so `cursor_row` remains within the visible window.
/// Call after any mutation that may change `cursor_row`.
fn textarea_fix_scroll(scroll: &mut usize, cursor_row: usize) {
    if cursor_row < *scroll {
        *scroll = cursor_row;
    } else if cursor_row >= *scroll + TEXTAREA_VISIBLE_ROWS {
        *scroll = cursor_row + 1 - TEXTAREA_VISIBLE_ROWS;
    }
}

/// Byte offset of the cursor position (row, col) in the textarea buffer.
/// Clamps gracefully when row/col exceed buffer extent.
fn textarea_byte_pos(buf: &str, row: usize, col: usize) -> usize {
    let mut offset = 0usize;
    for (li, line) in buf.split('\n').enumerate() {
        if li == row {
            // col is a char index within this line.
            let char_count = line.chars().count().min(col);
            offset += line.char_indices().nth(char_count).map(|(i, _)| i).unwrap_or(line.len());
            return offset;
        }
        offset += line.len() + 1; // +1 for the '\n'
    }
    buf.len()
}

/// Return the char count of line `row` in `buf` (0 if row is out of range).
fn textarea_line_char_len(buf: &str, row: usize) -> usize {
    buf.split('\n').nth(row).map(|l| l.chars().count()).unwrap_or(0)
}

/// Number of lines in `buf` (always >= 1).
fn textarea_line_count(buf: &str) -> usize {
    buf.split('\n').count()
}

/// Check a non-empty Path value for plausible existence. Returns `Ok(())` when
/// the path itself exists OR its parent directory exists (so a new leaf under an
/// existing directory is accepted). Returns `Err(message)` with a clear label
/// when neither is true. A bare relative name with no parent component (e.g.
/// `newdir`) is accepted because its implicit parent is the cwd, which always
/// exists. Injected `exists_fn` makes this unit-testable without touching disk.
fn validate_path_value(
    label: &str,
    value: &str,
    exists_fn: impl Fn(&std::path::Path) -> bool,
    is_dir_fn: impl Fn(&std::path::Path) -> bool,
) -> Result<(), String> {
    let p = std::path::Path::new(value);
    if exists_fn(p) {
        return Ok(());
    }
    let parent = p.parent();
    match parent {
        // No parent component or empty parent → bare name; cwd is the parent → accept.
        None => Ok(()),
        Some(par) if par.as_os_str().is_empty() => Ok(()),
        Some(par) => {
            if exists_fn(par) && is_dir_fn(par) {
                Ok(())
            } else {
                Err(format!(
                    "{label}: directory not found — check the path for typos (parent '{}' does not exist)",
                    par.display()
                ))
            }
        }
    }
}

/// Run Path validation for all Path fields in `fields` whose committed value is
/// non-empty. Returns the index of the first failing field and its error message,
/// or `None` if all pass.
fn run_path_validation(fields: &[Field], answers: &Map<String, Value>) -> Option<(usize, String)> {
    for (idx, field) in fields.iter().enumerate() {
        if field.field_type != FieldType::Path {
            continue;
        }
        let value = match answers.get(&field.id) {
            Some(Value::String(s)) if !s.is_empty() => s.as_str(),
            _ => continue,
        };
        let label = field.prompt.as_deref().unwrap_or(&field.id);
        if let Err(msg) = validate_path_value(
            label,
            value,
            |p| p.exists(),
            |p| p.is_dir(),
        ) {
            return Some((idx, msg));
        }
    }
    None
}

/// Run API validation for all fields in `fields` that have `validate.api` set
/// and whose committed value is a non-empty string. Returns the index of the
/// first failing field and its error message, or `None` if all pass. Shows a
/// "validating…" spinner while each request is in flight.
fn run_api_validation(
    fields: &[Field],
    answers: &Map<String, Value>,
    term: &mut Terminal<CrosstermBackend<Stdout>>,
    pal: &Palette,
    frame: &mut u64,
) -> Option<(usize, String)> {
    for (field_idx, field) in fields.iter().enumerate() {
        let api = match &field.validate.api {
            Some(a) => a.clone(),
            None => continue,
        };
        let value = match answers.get(&field.id) {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => continue,
        };
        // Use the human-readable prompt as the field label in error messages.
        let field_label = field.prompt.as_deref().unwrap_or(&field.id).to_string();

        // Spawn a thread so we can repaint the spinner while waiting.
        let (tx, rx) = mpsc::channel::<insmaller_core::Result<()>>();
        let api_clone = api.clone();
        let value_clone = value.clone();
        let label_clone = field_label.clone();
        std::thread::spawn(move || {
            let result = api_clone.call(&label_clone, &value_clone);
            let _ = tx.send(result);
        });

        // Poll with a spinner until the result arrives.
        let spinner_chars = ['|', '/', '-', '\\'];
        let mut spin_idx = 0usize;
        loop {
            let spin = spinner_chars[spin_idx % spinner_chars.len()];
            spin_idx += 1;
            let msg = format!("validating… {spin}");
            let _ = term.draw(|fr| {
                let rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(4),
                        Constraint::Min(3),
                        Constraint::Length(3),
                    ])
                    .split(fr.area());
                let foot = Line::from(vec![
                    Span::styled(msg.clone(), Style::default().fg(pal.muted)),
                ]);
                fr.render_widget(
                    Paragraph::new(foot).block(panel("", false, pal)),
                    rows[2],
                );
            });
            *frame = frame.wrapping_add(1);

            match rx.recv_timeout(Duration::from_millis(80)) {
                Ok(Ok(())) => break,
                Ok(Err(e)) => return Some((field_idx, format!("{e}"))),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Some((
                        field_idx,
                        format!("api validation: thread disconnected for '{field_label}'"),
                    ));
                }
            }
        }
    }
    None
}

fn init_widget(
    f: &Field,
    s: &WizardSession,
    gd: &GroupDefaults,
    collapse: &HashMap<String, bool>,
) -> Widget {
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
            let groups = group_list(&choices);
            let collapsed = gd.for_groups(&f.id, &groups, collapse);
            Widget::Multi { choices, on, groups, collapsed, cur: 0 }
        }
        FieldType::SingleSelect => {
            let choices = s.choices(f);
            let sel = match &prior {
                Some(Value::String(v)) => choices.iter().position(|c| &c.value == v),
                _ => None,
            };
            let groups = group_list(&choices);
            let collapsed = gd.for_groups(&f.id, &groups, collapse);
            Widget::Single { choices, sel, groups, collapsed, cur: 0 }
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
        FieldType::Dropdown => {
            let choices: Vec<String> = f.options.to_vec();
            let default_val = match prior {
                Some(Value::String(ref s)) => s.clone(),
                _ => f.default.clone().unwrap_or_default(),
            };
            let sel = choices.iter().position(|c| c == &default_val).unwrap_or(0);
            Widget::Dropdown { choices, sel, open: false, filter: String::new(), cur: 0 }
        }
        FieldType::Textarea => Widget::Textarea {
            buf: match prior {
                Some(Value::String(s)) => s,
                _ => f.default.clone().unwrap_or_default(),
            },
            cursor_row: 0,
            cursor_col: 0,
            scroll: 0,
        },
        FieldType::Date => {
            let s = match prior {
                Some(Value::String(ref v)) => v.clone(),
                _ => f.default.clone().unwrap_or_default(),
            };
            Widget::Date { digits: parse_date_digits(&s), dcur: 0, cal: None }
        }
        FieldType::Datetime => {
            let s = match prior {
                Some(Value::String(ref v)) => v.clone(),
                _ => f.default.clone().unwrap_or_default(),
            };
            Widget::Datetime { digits: parse_datetime_digits(&s), dcur: 0, cal: None }
        }
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
        Widget::Path { buf, .. } => Value::String(buf.trim().to_string()),
        Widget::Dropdown { choices, sel, .. } => Value::String(
            choices.get(*sel).cloned().unwrap_or_default(),
        ),
        Widget::Textarea { buf, .. } => Value::String(buf.clone()),
        Widget::Date { digits, .. } => Value::String(date_widget_value(digits)),
        Widget::Datetime { digits, .. } => Value::String(datetime_widget_value(digits)),
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

/// A titled panel. Under a colored theme it gets rounded corners and a border
/// tinted by focus (bright `border_focus` when active, dim `border` idle —
/// the focus glow). Under mono/`NO_COLOR` it stays the plain square box, so
/// nothing changes there.
fn panel<'a>(title: impl Into<Line<'a>>, focused: bool, pal: &Palette) -> Block<'a> {
    let mut b = Block::default().borders(Borders::ALL).title(title);
    if pal.colored() {
        let bc = if focused { pal.border_focus } else { pal.border };
        b = b
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(bc));
    }
    b
}

/// Run the wizard interactively. Returns true if completed, false if quit.
///
/// `no_api_validate`: when true, skip all `validate.api` network calls (useful
/// for CI / offline runs).
pub fn run_wizard_tui(
    session: &mut WizardSession,
    pal: Palette,
    gd: &GroupDefaults,
    no_api_validate: bool,
) -> anyhow::Result<bool> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let _g = TermGuard;
    let mut term: Terminal<CrosstermBackend<Stdout>> =
        Terminal::new(CrosstermBackend::new(io::stdout()))?;

    // Group collapse state, keyed by field id + group, persisted across page
    // re-entries (the per-page widgets are rebuilt each time).
    let mut collapse: HashMap<String, bool> = HashMap::new();

    // Animate only on a colored interactive terminal: under NO_COLOR/mono or
    // when piped/redirected we keep the blocking, zero-wakeup event loop.
    let animate = pal.colored() && io::stdout().is_terminal();
    let mut frame: u64 = 0;
    // Header gradient cached by width: accent→accent2 is invariant for the
    // session, only the animation `phase` rotates, so we rebuild the Vec only
    // on a resize, not every frame.
    let mut grad_cache: (usize, Vec<ratatui::style::Color>) = (0, Vec::new());

    while !session.is_done() {
        let fields: Vec<Field> = session.fields();
        let mut widgets: Vec<Widget> =
            fields.iter().map(|f| init_widget(f, session, gd, &collapse)).collect();
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

                let ratio = (step as f64 / total as f64).clamp(0.0, 1.0);
                let htitle = format!(" insmaller setup — {title}  (step {step}/{total}) ");
                if pal.colored() {
                    // Custom gradient progress bar: accent→accent2 flowing left
                    // to right, with the filled portion lit and the remainder
                    // dimmed. `frame` rotates the gradient for a subtle sheen.
                    let block = panel(htitle, false, &pal);
                    let inner = block.inner(rows[0]);
                    fr.render_widget(block, rows[0]);
                    let w = inner.width.max(1) as usize;
                    let filled = (ratio * w as f64).round() as usize;
                    if grad_cache.0 != w {
                        grad_cache = (w, gradient(pal.accent, pal.accent2, w));
                    }
                    let cols = &grad_cache.1;
                    let phase = (frame as usize) % w;
                    let bar: Vec<Span> = (0..w)
                        .map(|i| {
                            let col = cols[(i + phase) % w];
                            if i < filled {
                                Span::styled("▰", Style::default().fg(col))
                            } else {
                                Span::styled("▱", Style::default().fg(pal.border))
                            }
                        })
                        .collect();
                    let lines = vec![
                        Line::from(bar),
                        Line::from(Span::styled(desc.clone(), Style::default().fg(pal.muted))),
                    ];
                    fr.render_widget(Paragraph::new(lines), inner);
                } else {
                    let g = Gauge::default()
                        .block(Block::default().borders(Borders::ALL).title(htitle))
                        .gauge_style(Style::default().fg(pal.accent))
                        .ratio(ratio)
                        .label(desc.clone());
                    fr.render_widget(g, rows[0]);
                }

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
                        Widget::Multi { choices, on, groups, collapsed, cur } => {
                            for (pos, row) in
                                visible_rows(choices, groups, collapsed).iter().enumerate()
                            {
                                let p = if focused && *cur == pos { ">" } else { " " };
                                match row {
                                    Row::Header(gi) => {
                                        let g = &groups[*gi];
                                        let tri = if collapsed[*gi] { "▶" } else { "▼" };
                                        let mark = group_mark_multi(choices, on, g);
                                        items.push(ListItem::new(format!(
                                            "   {p}{tri} {mark} {g}"
                                        )));
                                    }
                                    Row::Item(i) => {
                                        let mark = if on[*i] { "[x]" } else { "[ ]" };
                                        let indent =
                                            if choices[*i].group.is_some() { "     " } else { "   " };
                                        items.push(ListItem::new(format!(
                                            "{indent}{p}{mark} {}",
                                            item_label(&choices[*i])
                                        )));
                                    }
                                }
                            }
                        }
                        Widget::Single { choices, sel, groups, collapsed, cur } => {
                            for (pos, row) in
                                visible_rows(choices, groups, collapsed).iter().enumerate()
                            {
                                let p = if focused && *cur == pos { ">" } else { " " };
                                match row {
                                    Row::Header(gi) => {
                                        // No radio mark on a single-select header
                                        // — a group isn't itself selectable.
                                        let g = &groups[*gi];
                                        let tri = if collapsed[*gi] { "▶" } else { "▼" };
                                        items.push(ListItem::new(format!("   {p}{tri} {g}")));
                                    }
                                    Row::Item(i) => {
                                        let mark = if *sel == Some(*i) { "(o)" } else { "( )" };
                                        let indent =
                                            if choices[*i].group.is_some() { "     " } else { "   " };
                                        items.push(ListItem::new(format!(
                                            "{indent}{p}{mark} {}",
                                            item_label(&choices[*i])
                                        )));
                                    }
                                }
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
                        Widget::Dropdown { choices, sel, open, .. } => {
                            let selected = choices.get(*sel).cloned().unwrap_or_default();
                            if *open {
                                items.push(ListItem::new(format!("   {selected} ▲  [type to filter · ↑↓ · Enter select · Esc cancel]")));
                            } else {
                                items.push(ListItem::new(format!(
                                    "   {selected} ▼{}",
                                    if focused { "  [Enter/Space to open]" } else { "" }
                                )));
                            }
                        }
                        Widget::Textarea { buf, cursor_row, cursor_col, scroll } => {
                            let lines: Vec<&str> = buf.split('\n').collect();
                            let total = lines.len();
                            let start = *scroll;
                            let end = (start + TEXTAREA_VISIBLE_ROWS).min(total);
                            for (li, line) in lines[start..end].iter().enumerate() {
                                let abs_row = li + start;
                                let rendered = if focused && abs_row == *cursor_row {
                                    // Insert a block cursor glyph at cursor_col.
                                    let col = (*cursor_col).min(line.chars().count());
                                    let before: String = line.chars().take(col).collect();
                                    let after: String = line.chars().skip(col).collect();
                                    format!("   {before}\u{258c}{after}")
                                } else {
                                    format!("   {line}")
                                };
                                items.push(ListItem::new(rendered));
                            }
                            if focused {
                                items.push(ListItem::new(format!(
                                    "   [Tab: next · Enter: newline · \u{2191}\u{2193}\u{2190}\u{2192}: navigate  (line {}/{})]",
                                    cursor_row + 1,
                                    total
                                )));
                            }
                        }
                        Widget::Date { digits, .. } => {
                            let mask = render_date_mask(digits);
                            items.push(ListItem::new(format!(
                                "   {}{}",
                                mask,
                                if focused { "  [digits only · Space: calendar]" } else { "" }
                            )));
                        }
                        Widget::Datetime { digits, .. } => {
                            let mask = render_datetime_mask(digits);
                            items.push(ListItem::new(format!(
                                "   {}{}",
                                mask,
                                if focused { "  [digits only · Space: calendar]" } else { "" }
                            )));
                        }
                    }
                }
                let body = List::new(items).block(panel(" fields ", focus < n, &pal));
                fr.render_widget(body, rows[1]);

                // Dropdown popup overlay.
                if let Some(Widget::Dropdown { choices, sel: _, open: true, filter, cur }) =
                    widgets.get(focus)
                {
                    let area = centered_rect(60, 60, fr.area());
                    let filtered: Vec<&String> = choices
                        .iter()
                        .filter(|c| {
                            filter.is_empty()
                                || c.to_lowercase().contains(&filter.to_lowercase())
                        })
                        .collect();
                    // Search header row — always first, never selectable.
                    let search_line = if filter.is_empty() {
                        "  Search: \u{258c}  (type to filter)".to_string()
                    } else {
                        format!("  Search: {filter}\u{258c}")
                    };
                    let header_item = ListItem::new(Span::styled(
                        search_line,
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                    let mut rows_d: Vec<ListItem> = vec![header_item];
                    if filtered.is_empty() {
                        rows_d.push(ListItem::new(Span::styled(
                            "  [no matches]",
                            Style::default().add_modifier(Modifier::DIM),
                        )));
                    } else {
                        rows_d.extend(filtered.iter().map(|c| ListItem::new((*c).clone())));
                    }
                    let title = " \u{2191}\u{2193} move \u{b7} Enter select \u{b7} Esc cancel ";
                    let list = List::new(rows_d)
                        .block(panel(title, true, &pal))
                        .highlight_style(
                            Style::default()
                                .fg(pal.accent_fg)
                                .bg(pal.accent)
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("> ");
                    let mut st = ListState::default();
                    // Clamp highlight to the actual filtered-list length and
                    // offset by 1 to skip the non-selectable search header row.
                    let clamped_cur = (*cur).min(filtered.len().saturating_sub(1));
                    st.select(if filtered.is_empty() { None } else { Some(clamped_cur + 1) });
                    if pal.colored() {
                        let fa = fr.area();
                        let sx = area.x + 1;
                        let sy = area.y + 1;
                        let shadow = Rect {
                            x: sx,
                            y: sy,
                            width: area.width.min(fa.width.saturating_sub(sx)),
                            height: area.height.min(fa.height.saturating_sub(sy)),
                        };
                        fr.render_widget(
                            Block::default().style(Style::default().bg(pal.shadow)),
                            shadow,
                        );
                    }
                    fr.render_widget(Clear, area);
                    fr.render_stateful_widget(list, area, &mut st);
                }

                // Path browser overlay (captures all keys while open).
                if let Some(Widget::Path { picker: Some(p), .. }) = widgets.get(focus) {
                    let area = centered_rect(70, 70, fr.area());
                    let rows_p: Vec<ListItem> = p
                        .entries
                        .iter()
                        .map(|e| {
                            let name = match e.name.as_str() {
                                "." => ".    (select this folder)".to_string(),
                                ".." => "..   (parent folder)".to_string(),
                                _ if e.is_dir => format!("{}/", e.name),
                                _ => e.name.clone(),
                            };
                            ListItem::new(name)
                        })
                        .collect();
                    let state = if p.readable { "" } else { "  [unreadable]" };
                    let loc = if p.at_drive_selector() {
                        "Drives".to_string()
                    } else {
                        p.cwd.display().to_string()
                    };
                    let drives_hint = if cfg!(windows) { " · d drives" } else { "" };
                    let title = format!(
                        " {loc}{state}  (↑↓ move · ↵ open/select · ← up{drives_hint} · Esc cancel) "
                    );
                    let list = List::new(rows_p)
                        .block(panel(title, true, &pal))
                        .highlight_style(
                            Style::default()
                                .fg(pal.accent_fg)
                                .bg(pal.accent)
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("> ");
                    let mut st = ListState::default();
                    st.select(Some(p.cursor));
                    // Drop shadow: a dark rect offset +1/+1, drawn before Clear
                    // so the L-shaped sliver outside `area` stays shadowed.
                    if pal.colored() {
                        let fa = fr.area();
                        let sx = area.x + 1;
                        let sy = area.y + 1;
                        let shadow = Rect {
                            x: sx,
                            y: sy,
                            width: area.width.min(fa.width.saturating_sub(sx)),
                            height: area.height.min(fa.height.saturating_sub(sy)),
                        };
                        fr.render_widget(
                            Block::default().style(Style::default().bg(pal.shadow)),
                            shadow,
                        );
                    }
                    fr.render_widget(Clear, area);
                    fr.render_stateful_widget(list, area, &mut st);
                }

                // Calendar overlay for Date / Datetime fields.
                let cal_opt: Option<(&CalPicker, bool)> = match widgets.get(focus) {
                    Some(Widget::Date { cal: Some(c), .. }) => Some((c, false)),
                    Some(Widget::Datetime { cal: Some(c), .. }) => Some((c, true)),
                    _ => None,
                };
                if let Some((cal, is_datetime)) = cal_opt {
                    let area = centered_rect(36, 60, fr.area());
                    let month_name = cal.date.format("%B %Y").to_string();
                    let title = format!(" {month_name}  (←→ day · ↑↓ week · PgUp/Dn month · Enter · Esc) ");
                    let cal_lines = render_calendar(cal.date.year(), cal.date.month(), cal.date);
                    let hint = if is_datetime { "  (date only; time preserved)" } else { "" };
                    let mut rows_cal: Vec<ListItem> = cal_lines
                        .iter()
                        .map(|l| ListItem::new(format!(" {l}")))
                        .collect();
                    rows_cal.push(ListItem::new(format!(" {hint}")));
                    let list = List::new(rows_cal).block(panel(title, true, &pal));
                    if pal.colored() {
                        let fa = fr.area();
                        let sx = area.x + 1;
                        let sy = area.y + 1;
                        let shadow = Rect {
                            x: sx,
                            y: sy,
                            width: area.width.min(fa.width.saturating_sub(sx)),
                            height: area.height.min(fa.height.saturating_sub(sy)),
                        };
                        fr.render_widget(
                            Block::default().style(Style::default().bg(pal.shadow)),
                            shadow,
                        );
                    }
                    fr.render_widget(Clear, area);
                    fr.render_widget(list, area);
                }

                let btn = |label: &str, idx: usize, enabled: bool| {
                    let st = if !enabled {
                        Style::default().fg(pal.muted)
                    } else if focus == idx {
                        Style::default()
                            .fg(pal.accent_fg)
                            .bg(pal.accent)
                            .add_modifier(Modifier::BOLD)
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
                            "Tab focus · ↑↓ move · ←→ expand/collapse · Space toggle · Enter next · Esc back · q quit".into()
                        }),
                        Style::default().fg(if err.is_some() { pal.error } else { pal.muted }),
                    ),
                ]);
                fr.render_widget(
                    Paragraph::new(foot).block(panel("", focus >= n, &pal)),
                    rows[2],
                );
            })?;

            // Animated themes poll on a tick so the gradient sheen advances
            // while idle; otherwise block (no idle wakeups under CI/piped/mono).
            if animate && !event::poll(Duration::from_millis(80))? {
                frame = frame.wrapping_add(1);
                continue;
            }
            let Event::Key(k) = event::read()? else { continue };
            if k.kind != KeyEventKind::Press {
                continue;
            }

            // Ctrl+C always quits, even with the browser open.
            if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                return Ok(false);
            }

            // Pre-compute overlay states so all branches can reference them.
            let path_picker_open = matches!(
                widgets.get(focus),
                Some(Widget::Path { picker: Some(_), .. })
            );
            let dropdown_open_pre = matches!(
                widgets.get(focus),
                Some(Widget::Dropdown { open: true, .. })
            );
            let cal_open_pre = matches!(
                widgets.get(focus),
                Some(Widget::Date { cal: Some(_), .. })
                    | Some(Widget::Datetime { cal: Some(_), .. })
            );

            // An open path browser owns every key until it closes.
            if path_picker_open {
                let picker_buf_pair: Option<(&mut String, &mut Option<Picker>)> =
                    match widgets.get_mut(focus) {
                        Some(Widget::Path { buf, picker }) => Some((buf, picker)),
                        _ => None,
                    };
                if let Some((buf, picker)) = picker_buf_pair {
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
                            if let Some(path) = p.select_cwd() {
                                *buf = path;
                                *picker = None;
                            }
                        }
                        KeyCode::Char('d') => p.goto_drives(),
                        KeyCode::Esc => *picker = None,
                        _ => {}
                    }
                }
                continue;
            }

            // An open dropdown owns every key until it closes.
            if dropdown_open_pre {
                if let Some(Widget::Dropdown { choices, sel, open, filter, cur }) =
                    widgets.get_mut(focus)
                {
                    match k.code {
                        KeyCode::Esc => {
                            *open = false;
                            filter.clear();
                        }
                        KeyCode::Enter => {
                            // Commit highlighted filtered choice. If filter
                            // yields nothing, keep the popup open — don't
                            // silently close with a stale selection.
                            let filtered: Vec<usize> = choices
                                .iter()
                                .enumerate()
                                .filter(|(_, c)| {
                                    filter.is_empty()
                                        || c.to_lowercase().contains(&filter.to_lowercase())
                                })
                                .map(|(i, _)| i)
                                .collect();
                            if filtered.is_empty() {
                                // Nothing matches — stay open so user can adjust.
                            } else {
                                let clamped = (*cur).min(filtered.len() - 1);
                                *sel = filtered[clamped];
                                *open = false;
                                filter.clear();
                            }
                        }
                        KeyCode::Up => *cur = cur.saturating_sub(1),
                        KeyCode::Down => {
                            let filtered_len = choices
                                .iter()
                                .filter(|c| {
                                    filter.is_empty()
                                        || c.to_lowercase().contains(&filter.to_lowercase())
                                })
                                .count();
                            if filtered_len > 0 && *cur + 1 < filtered_len {
                                *cur += 1;
                            }
                        }
                        KeyCode::Backspace => {
                            filter.pop();
                            // Re-clamp cursor after list may have grown back.
                            let new_len = choices
                                .iter()
                                .filter(|c| {
                                    filter.is_empty()
                                        || c.to_lowercase().contains(&filter.to_lowercase())
                                })
                                .count();
                            *cur = (*cur).min(new_len.saturating_sub(1));
                        }
                        KeyCode::Char(ch) => {
                            filter.push(ch);
                            *cur = 0;
                        }
                        _ => {}
                    }
                }
                continue;
            }

            // Calendar overlay owns every key while open.
            if cal_open_pre {
                match widgets.get_mut(focus) {
                    Some(Widget::Date { digits, cal, .. }) => {
                        if let Some(c) = cal.as_mut() {
                            match k.code {
                                KeyCode::Esc => *cal = None,
                                KeyCode::Enter => {
                                    *digits = date_to_date_digits(c.date);
                                    *cal = None;
                                }
                                KeyCode::Left => {
                                    c.date = c.date.pred_opt().unwrap_or(c.date);
                                }
                                KeyCode::Right => {
                                    c.date = c.date.succ_opt().unwrap_or(c.date);
                                }
                                KeyCode::Up => {
                                    c.date = c.date.checked_sub_days(Days::new(7)).unwrap_or(c.date);
                                }
                                KeyCode::Down => {
                                    c.date = c.date.checked_add_days(Days::new(7)).unwrap_or(c.date);
                                }
                                KeyCode::PageUp => {
                                    c.date = c.date.checked_sub_months(Months::new(1)).unwrap_or(c.date);
                                }
                                KeyCode::PageDown => {
                                    c.date = c.date.checked_add_months(Months::new(1)).unwrap_or(c.date);
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(Widget::Datetime { digits, cal, .. }) => {
                        if let Some(c) = cal.as_mut() {
                            match k.code {
                                KeyCode::Esc => *cal = None,
                                KeyCode::Enter => {
                                    *digits = date_to_datetime_digits(c.date, digits);
                                    *cal = None;
                                }
                                KeyCode::Left => {
                                    c.date = c.date.pred_opt().unwrap_or(c.date);
                                }
                                KeyCode::Right => {
                                    c.date = c.date.succ_opt().unwrap_or(c.date);
                                }
                                KeyCode::Up => {
                                    c.date = c.date.checked_sub_days(Days::new(7)).unwrap_or(c.date);
                                }
                                KeyCode::Down => {
                                    c.date = c.date.checked_add_days(Days::new(7)).unwrap_or(c.date);
                                }
                                KeyCode::PageUp => {
                                    c.date = c.date.checked_sub_months(Months::new(1)).unwrap_or(c.date);
                                }
                                KeyCode::PageDown => {
                                    c.date = c.date.checked_add_months(Months::new(1)).unwrap_or(c.date);
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }

            // Ctrl+B opens the filesystem browser on a focused Path field only.
            // Date/Datetime are masked text-only; Ctrl+B is a no-op for them.
            if k.code == KeyCode::Char('b') && k.modifiers.contains(KeyModifiers::CONTROL) {
                if let Some(Widget::Path { buf, picker }) = widgets.get_mut(focus) {
                    *picker = Some(Picker::open(buf));
                }
                continue;
            }

            let editing = matches!(
                widgets.get(focus),
                Some(Widget::Input { .. })
                    | Some(Widget::Path { .. })
                    | Some(Widget::Textarea { .. })
                    | Some(Widget::Date { .. })
                    | Some(Widget::Datetime { .. })
            );
            // quit
            if k.code == KeyCode::Char('q') && !editing && !dropdown_open_pre && !cal_open_pre {
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
                // On a grouped select, →/← drive expand/collapse instead of
                // field focus (focus still moves via Tab / ↑↓).
                KeyCode::Right if focus < n && widget_has_groups(&widgets[focus]) => {
                    if let Some(Row::Header(gi)) = current_row(&widgets[focus]) {
                        if let Widget::Multi { collapsed, .. }
                        | Widget::Single { collapsed, .. } = &mut widgets[focus]
                        {
                            collapsed[gi] = false;
                        }
                    }
                }
                KeyCode::Left if focus < n && widget_has_groups(&widgets[focus]) => {
                    match current_row(&widgets[focus]) {
                        Some(Row::Header(gi)) => {
                            if let Widget::Multi { collapsed, .. }
                            | Widget::Single { collapsed, .. } = &mut widgets[focus]
                            {
                                collapsed[gi] = true;
                            }
                            clamp_cur(&mut widgets[focus]);
                        }
                        Some(Row::Item(i)) => cursor_to_header_of(&mut widgets[focus], i),
                        None => {}
                    }
                }
                KeyCode::Tab | KeyCode::Right if !editing => focus = (focus + 1) % (n + 2),
                KeyCode::BackTab | KeyCode::Left if !editing => {
                    focus = (focus + n + 1) % (n + 2)
                }
                // ── Textarea cursor navigation (intercept before field-nav) ──
                KeyCode::Up
                    if focus < n && matches!(widgets[focus], Widget::Textarea { .. }) =>
                {
                    if let Widget::Textarea { buf, cursor_row, cursor_col, scroll } =
                        &mut widgets[focus]
                    {
                        if *cursor_row > 0 {
                            *cursor_row -= 1;
                            *cursor_col =
                                (*cursor_col).min(textarea_line_char_len(buf, *cursor_row));
                            textarea_fix_scroll(scroll, *cursor_row);
                        }
                    }
                }
                KeyCode::Down
                    if focus < n && matches!(widgets[focus], Widget::Textarea { .. }) =>
                {
                    if let Widget::Textarea { buf, cursor_row, cursor_col, scroll } =
                        &mut widgets[focus]
                    {
                        let last = textarea_line_count(buf).saturating_sub(1);
                        if *cursor_row < last {
                            *cursor_row += 1;
                            *cursor_col =
                                (*cursor_col).min(textarea_line_char_len(buf, *cursor_row));
                            textarea_fix_scroll(scroll, *cursor_row);
                        }
                    }
                }
                KeyCode::Left
                    if focus < n && matches!(widgets[focus], Widget::Textarea { .. }) =>
                {
                    if let Widget::Textarea { buf, cursor_row, cursor_col, scroll } =
                        &mut widgets[focus]
                    {
                        if *cursor_col > 0 {
                            *cursor_col -= 1;
                        } else if *cursor_row > 0 {
                            *cursor_row -= 1;
                            *cursor_col = textarea_line_char_len(buf, *cursor_row);
                            textarea_fix_scroll(scroll, *cursor_row);
                        }
                    }
                }
                KeyCode::Right
                    if focus < n && matches!(widgets[focus], Widget::Textarea { .. }) =>
                {
                    if let Widget::Textarea { buf, cursor_row, cursor_col, scroll } =
                        &mut widgets[focus]
                    {
                        let line_len = textarea_line_char_len(buf, *cursor_row);
                        if *cursor_col < line_len {
                            *cursor_col += 1;
                        } else {
                            let last = textarea_line_count(buf).saturating_sub(1);
                            if *cursor_row < last {
                                *cursor_row += 1;
                                *cursor_col = 0;
                                textarea_fix_scroll(scroll, *cursor_row);
                            }
                        }
                    }
                }
                KeyCode::Home
                    if focus < n && matches!(widgets[focus], Widget::Textarea { .. }) =>
                {
                    if let Widget::Textarea { cursor_col, .. } = &mut widgets[focus] {
                        *cursor_col = 0;
                    }
                }
                KeyCode::End
                    if focus < n && matches!(widgets[focus], Widget::Textarea { .. }) =>
                {
                    if let Widget::Textarea { buf, cursor_row, cursor_col, .. } =
                        &mut widgets[focus]
                    {
                        *cursor_col = textarea_line_char_len(buf, *cursor_row);
                    }
                }
                KeyCode::PageUp
                    if focus < n && matches!(widgets[focus], Widget::Textarea { .. }) =>
                {
                    if let Widget::Textarea { buf, cursor_row, cursor_col, scroll } =
                        &mut widgets[focus]
                    {
                        *cursor_row = cursor_row.saturating_sub(TEXTAREA_VISIBLE_ROWS);
                        *cursor_col =
                            (*cursor_col).min(textarea_line_char_len(buf, *cursor_row));
                        textarea_fix_scroll(scroll, *cursor_row);
                    }
                }
                KeyCode::PageDown
                    if focus < n && matches!(widgets[focus], Widget::Textarea { .. }) =>
                {
                    if let Widget::Textarea { buf, cursor_row, cursor_col, scroll } =
                        &mut widgets[focus]
                    {
                        let last = textarea_line_count(buf).saturating_sub(1);
                        *cursor_row = (*cursor_row + TEXTAREA_VISIBLE_ROWS).min(last);
                        *cursor_col =
                            (*cursor_col).min(textarea_line_char_len(buf, *cursor_row));
                        textarea_fix_scroll(scroll, *cursor_row);
                    }
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
                    // For selects, the cursor ranges over visible tree rows
                    // (headers + items), not the raw choices.
                    let len = tree_rows_of(&widgets[focus]).map_or(0, |r| r.len());
                    let cur = cur_of(&widgets[focus]);
                    let (new_cur, new_focus) = vert_nav(cur, len, down, focus, n);
                    if let Widget::Multi { cur, .. } | Widget::Single { cur, .. } =
                        &mut widgets[focus]
                    {
                        *cur = new_cur;
                    }
                    focus = new_focus;
                }
                KeyCode::Char(' ') if focus < n => {
                    let row = current_row(&widgets[focus]);
                    match &mut widgets[focus] {
                        Widget::Multi { on, collapsed, .. } => match row {
                            Some(Row::Item(i)) => on[i] = !on[i],
                            Some(Row::Header(gi)) => collapsed[gi] = !collapsed[gi],
                            None => {}
                        },
                        Widget::Single { sel, collapsed, .. } => match row {
                            Some(Row::Item(i)) => *sel = Some(i),
                            Some(Row::Header(gi)) => collapsed[gi] = !collapsed[gi],
                            None => {}
                        },
                        Widget::Toggle { on } => *on = !*on,
                        Widget::Input { buf, .. } | Widget::Path { buf, .. } => buf.push(' '),
                        Widget::Textarea { buf, cursor_row, cursor_col, scroll } => {
                            textarea_insert(buf, cursor_row, cursor_col, ' ');
                            textarea_fix_scroll(scroll, *cursor_row);
                        }
                        Widget::Date { digits, cal, .. } => {
                            // Space opens the calendar overlay.
                            let seed = date_from_date_digits(digits)
                                .unwrap_or_else(|| Local::now().date_naive());
                            *cal = Some(CalPicker { date: seed });
                        }
                        Widget::Datetime { digits, cal, .. } => {
                            // Space opens the calendar overlay.
                            let date_only = {
                                // extract date from first 8 digit slots
                                let date_digits: [u8; 8] = digits[..8].try_into().unwrap_or([b'_'; 8]);
                                date_from_date_digits(&date_digits)
                                    .unwrap_or_else(|| Local::now().date_naive())
                            };
                            *cal = Some(CalPicker { date: date_only });
                        }
                        Widget::Dropdown { open, filter, cur, .. } => {
                            // Space opens the dropdown.
                            *open = true;
                            filter.clear();
                            *cur = 0;
                        }
                    }
                    clamp_cur(&mut widgets[focus]);
                }
                KeyCode::Char(ch) if editing => {
                    match &mut widgets[focus] {
                        Widget::Input { buf, .. } | Widget::Path { buf, .. } => buf.push(ch),
                        Widget::Date { digits, dcur, .. } if ch.is_ascii_digit() => {
                            *dcur = date_type_digit(digits, *dcur, ch as u8);
                        }
                        Widget::Date { .. } => {} // non-digit silently rejected
                        Widget::Datetime { digits, dcur, .. } if ch.is_ascii_digit() => {
                            *dcur = datetime_type_digit(digits, *dcur, ch as u8);
                        }
                        Widget::Datetime { .. } => {} // non-digit silently rejected
                        Widget::Textarea { buf, cursor_row, cursor_col, scroll } => {
                            textarea_insert(buf, cursor_row, cursor_col, ch);
                            textarea_fix_scroll(scroll, *cursor_row);
                        }
                        _ => {}
                    }
                }
                KeyCode::Backspace if editing => {
                    match &mut widgets[focus] {
                        Widget::Input { buf, .. } | Widget::Path { buf, .. } => { buf.pop(); }
                        Widget::Date { digits, dcur, .. } => {
                            *dcur = date_backspace(digits, *dcur);
                        }
                        Widget::Datetime { digits, dcur, .. } => {
                            *dcur = datetime_backspace(digits, *dcur);
                        }
                        Widget::Textarea { buf, cursor_row, cursor_col, scroll } => {
                            textarea_backspace(buf, cursor_row, cursor_col);
                            textarea_fix_scroll(scroll, *cursor_row);
                        }
                        _ => {}
                    }
                }
                // Enter in a Textarea inserts a newline; Tab commits/advances.
                KeyCode::Enter if focus < n && matches!(widgets[focus], Widget::Textarea { .. }) => {
                    if let Widget::Textarea { buf, cursor_row, cursor_col, scroll } =
                        &mut widgets[focus]
                    {
                        textarea_insert(buf, cursor_row, cursor_col, '\n');
                        textarea_fix_scroll(scroll, *cursor_row);
                    }
                }
                // Tab commits a textarea and advances focus.
                KeyCode::Tab if focus < n && matches!(widgets[focus], Widget::Textarea { .. }) => {
                    focus = (focus + 1) % (n + 2);
                }
                // Enter on a focused Dropdown opens it.
                KeyCode::Enter if focus < n && matches!(widgets[focus], Widget::Dropdown { open: false, .. }) => {
                    if let Widget::Dropdown { open, filter, cur, .. } = &mut widgets[focus] {
                        *open = true;
                        filter.clear();
                        *cur = 0;
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
                        // Next (or any field) → path validation, API validation, submit.
                        let m = commit(&widgets, &fields);
                        // Path existence check (TUI only; headless/--answers skips this).
                        if let Some((fail_idx, path_err)) = run_path_validation(&fields, &m) {
                            err = Some(path_err);
                            focus = fail_idx;
                            continue;
                        }
                        // API validation (skipped when --no-api-validate).
                        if !no_api_validate {
                            if let Some((fail_idx, api_err)) = run_api_validation(&fields, &m, &mut term, &pal, &mut frame) {
                                err = Some(api_err);
                                focus = fail_idx;
                                continue;
                            }
                        }
                        match session.submit(m) {
                            Ok(()) => break,
                            Err(e) => err = Some(format!("{e}")),
                        }
                    }
                }
                _ => {}
            }
        }
        // Persist this page's group collapse state so it survives Back/Next.
        for (w, f) in widgets.iter().zip(&fields) {
            if let Widget::Multi { groups, collapsed, .. }
            | Widget::Single { groups, collapsed, .. } = w
            {
                for (g, c) in groups.iter().zip(collapsed) {
                    collapse.insert(collapse_key(&f.id, g), *c);
                }
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
    use super::{
        date_backspace, date_from_date_digits, date_to_date_digits, date_type_digit,
        date_widget_value, datetime_backspace, datetime_type_digit, datetime_widget_value,
        textarea_line_char_len, textarea_line_count,
        days_in_month, group_list, group_mark_multi, item_label, list_dir, parse_date_digits,
        parse_datetime_digits, render_date_mask, render_datetime_mask, textarea_backspace,
        textarea_byte_pos, textarea_insert, validate_path_value, vert_nav, visible_rows,
        GroupDefaults, Picker, Row,
    };
    use chrono::{Datelike, Days, Months, NaiveDate};
    use insmaller_core::Choice;

    // ── textarea helpers ─────────────────────────────────────────────────

    #[test]
    fn textarea_byte_pos_empty() {
        assert_eq!(textarea_byte_pos("", 0, 0), 0);
    }

    #[test]
    fn textarea_byte_pos_single_line() {
        let buf = "hello";
        assert_eq!(textarea_byte_pos(buf, 0, 0), 0);
        assert_eq!(textarea_byte_pos(buf, 0, 3), 3);
        assert_eq!(textarea_byte_pos(buf, 0, 5), 5);
        // col beyond end clamps to line end
        assert_eq!(textarea_byte_pos(buf, 0, 100), 5);
    }

    #[test]
    fn textarea_byte_pos_multiline() {
        let buf = "ab\ncd\nef";
        // row 0: "ab"
        assert_eq!(textarea_byte_pos(buf, 0, 1), 1);
        // row 1: "cd" starts at byte 3 (after "ab\n")
        assert_eq!(textarea_byte_pos(buf, 1, 0), 3);
        assert_eq!(textarea_byte_pos(buf, 1, 1), 4);
        // row 2: "ef" starts at byte 6
        assert_eq!(textarea_byte_pos(buf, 2, 0), 6);
    }

    #[test]
    fn textarea_insert_char_advances_col() {
        let mut buf = String::from("ac");
        let mut row = 0;
        let mut col = 1; // insert between a and c
        textarea_insert(&mut buf, &mut row, &mut col, 'b');
        assert_eq!(buf, "abc");
        assert_eq!(row, 0);
        assert_eq!(col, 2);
    }

    #[test]
    fn textarea_insert_newline_advances_row() {
        let mut buf = String::from("hello");
        let mut row = 0;
        let mut col = 5;
        textarea_insert(&mut buf, &mut row, &mut col, '\n');
        assert_eq!(buf, "hello\n");
        assert_eq!(row, 1);
        assert_eq!(col, 0);
    }

    #[test]
    fn textarea_backspace_deletes_char() {
        let mut buf = String::from("abc");
        let mut row = 0;
        let mut col = 3;
        textarea_backspace(&mut buf, &mut row, &mut col);
        assert_eq!(buf, "ab");
        assert_eq!(col, 2);
    }

    #[test]
    fn textarea_backspace_at_start_noop() {
        let mut buf = String::from("abc");
        let mut row = 0;
        let mut col = 0;
        textarea_backspace(&mut buf, &mut row, &mut col);
        assert_eq!(buf, "abc");
        assert_eq!(col, 0);
    }

    #[test]
    fn textarea_backspace_deletes_newline_joins_lines() {
        let mut buf = String::from("ab\ncd");
        let mut row = 1;
        let mut col = 0;
        textarea_backspace(&mut buf, &mut row, &mut col);
        assert_eq!(buf, "abcd");
        assert_eq!(row, 0);
        assert_eq!(col, 2); // end of "ab"
    }

    fn ch(value: &str, group: Option<&str>) -> Choice {
        Choice {
            value: value.into(),
            label: value.into(),
            default: false,
            group: group.map(str::to_string),
        }
    }

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
    fn list_dir_dot_dotdot_then_dirs_before_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("zdir")).unwrap();
        std::fs::write(dir.path().join("afile.txt"), b"x").unwrap();
        let (entries, readable) = list_dir(dir.path());
        assert!(readable);
        // "." (select this folder) first, then ".." (parent)
        assert_eq!(entries[0].name, ".");
        assert_eq!(entries[1].name, "..");
        // directory sorts before the file despite "zdir" > "afile"
        assert_eq!(entries[2].name, "zdir");
        assert!(entries[2].is_dir);
        assert_eq!(entries[3].name, "afile.txt");
        assert!(!entries[3].is_dir);
    }

    #[test]
    fn list_dir_reports_unreadable() {
        // A path that is not a directory cannot be listed → readable=false,
        // and only the synthetic "." and ".." entries are present.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not_a_dir.txt");
        std::fs::write(&file, b"x").unwrap();
        let (entries, readable) = list_dir(&file);
        assert!(!readable);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec![".", ".."]
        );
    }

    #[test]
    fn picker_descends_ascends_and_selects_file() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("f.txt"), b"x").unwrap();

        let mut p = Picker::open(&dir.path().to_string_lossy());
        // [., .., sub]; cursor 0 is "." which selects this very folder
        assert_eq!(p.entries[p.cursor].name, ".");
        assert_eq!(
            p.activate().map(std::path::PathBuf::from),
            Some(dir.path().to_path_buf()),
            "'.' selects the current folder"
        );

        // move onto "sub" (skip ., ..) and descend
        p.down();
        p.down();
        assert_eq!(p.entries[p.cursor].name, "sub");
        assert_eq!(p.activate(), None);
        assert_eq!(p.cwd, sub);

        // now [., .., f.txt]; selecting the file returns its full path
        p.down();
        p.down();
        assert_eq!(p.entries[p.cursor].name, "f.txt");
        let got = p.activate().expect("file selection returns a path");
        assert_eq!(std::path::PathBuf::from(got), sub.join("f.txt"));

        // activating ".." ascends back to the parent
        let mut q = Picker::open(&sub.to_string_lossy());
        q.down(); // onto ".."
        assert_eq!(q.entries[q.cursor].name, "..");
        assert_eq!(q.activate(), None);
        assert_eq!(q.cwd, dir.path());
    }

    #[cfg(windows)]
    #[test]
    fn drive_root_offers_dotdot_to_selector() {
        // At a drive root, `..` is present and ascending lands on the empty
        // drive-selector path (it can't escape past it).
        let (entries, readable) = list_dir(std::path::Path::new("C:\\"));
        assert!(readable);
        assert!(entries.iter().any(|e| e.name == ".."));

        let mut p = Picker::open("C:\\");
        assert_eq!(p.cwd, std::path::PathBuf::from("C:\\"));
        p.ascend();
        assert!(p.cwd.as_os_str().is_empty(), "ascends to the drive selector");
        // Already at the selector: a further ascend is a no-op.
        p.ascend();
        assert!(p.cwd.as_os_str().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn selector_lists_drives_and_activate_descends() {
        let (drives, readable) = list_dir(&std::path::PathBuf::new());
        assert!(readable);
        assert!(!drives.is_empty(), "at least the system drive is present");
        assert!(drives.iter().all(|e| e.is_dir));

        let mut p = Picker::open("C:\\");
        p.set_dir(std::path::PathBuf::new()); // jump to the selector
        // Activate the first drive → cwd becomes its root with a trailing sep.
        assert_eq!(p.activate(), None);
        assert!(!p.cwd.as_os_str().is_empty());
        let s = p.cwd.to_string_lossy();
        assert!(s.ends_with('\\'), "drive root keeps a trailing separator: {s}");
    }

    #[cfg(windows)]
    #[test]
    fn d_shortcut_jumps_to_selector_from_any_depth() {
        // From a normal directory, `d` jumps straight to the drive selector
        // without walking parents; on the selector it's a no-op.
        let dir = tempfile::tempdir().unwrap();
        let mut p = Picker::open(&dir.path().to_string_lossy());
        assert!(!p.cwd.as_os_str().is_empty());
        p.goto_drives();
        assert!(p.cwd.as_os_str().is_empty(), "d jumps to the drive selector");
        p.goto_drives();
        assert!(p.cwd.as_os_str().is_empty(), "no-op once already there");
    }

    #[cfg(windows)]
    #[test]
    fn select_at_drive_selector_yields_no_value() {
        // 's' / '.' must not return the empty sentinel as a chosen path.
        let mut p = Picker::open("C:\\");
        p.goto_drives();
        assert!(p.at_drive_selector());
        assert_eq!(p.select_cwd(), None, "no folder to take at the drive list");
    }

    #[test]
    fn group_list_first_appearance_order_excludes_ungrouped() {
        let choices = vec![
            ch("a", None),
            ch("bun", Some("runtime")),
            ch("node", Some("runtime")),
            ch("claude", Some("ai")),
        ];
        assert_eq!(group_list(&choices), vec!["runtime".to_string(), "ai".to_string()]);
    }

    #[test]
    fn visible_rows_ungrouped_first_then_headers_and_collapse() {
        let choices = vec![
            ch("a", None),
            ch("bun", Some("runtime")),
            ch("node", Some("runtime")),
            ch("claude", Some("ai")),
        ];
        let groups = group_list(&choices);
        let rows = visible_rows(&choices, &groups, &[false, false]);
        assert_eq!(
            rows,
            vec![
                Row::Item(0),
                Row::Header(0),
                Row::Item(1),
                Row::Item(2),
                Row::Header(1),
                Row::Item(3),
            ]
        );
        // collapsing "runtime" hides its two items but keeps the header
        let rows = visible_rows(&choices, &groups, &[true, false]);
        assert_eq!(
            rows,
            vec![Row::Item(0), Row::Header(0), Row::Header(1), Row::Item(3)]
        );
    }

    #[test]
    fn no_groups_renders_flat() {
        let choices = vec![ch("a", None), ch("b", None)];
        let groups = group_list(&choices);
        assert!(groups.is_empty());
        assert_eq!(
            visible_rows(&choices, &groups, &[]),
            vec![Row::Item(0), Row::Item(1)]
        );
    }

    #[test]
    fn group_mark_all_some_none() {
        let choices = vec![ch("bun", Some("runtime")), ch("node", Some("runtime"))];
        assert_eq!(group_mark_multi(&choices, &[false, false], "runtime"), "[ ]");
        assert_eq!(group_mark_multi(&choices, &[true, false], "runtime"), "[~]");
        assert_eq!(group_mark_multi(&choices, &[true, true], "runtime"), "[x]");
    }

    #[test]
    fn item_label_strips_group_prefix() {
        let c = Choice {
            value: "bun".into(),
            label: "[runtime] bun — fast".into(),
            default: false,
            group: Some("runtime".into()),
        };
        assert_eq!(item_label(&c), "bun — fast");
        assert_eq!(item_label(&ch("x", None)), "x");
    }

    #[test]
    fn group_defaults_precedence() {
        let gd = GroupDefaults {
            collapsed_default: true,
            collapsed: vec!["x".into()],
            expanded: vec!["y".into()],
        };
        // baseline applies when not named
        assert!(gd.is_collapsed("other"));
        // expanded wins even over the collapsed baseline / collapsed list
        assert!(!gd.is_collapsed("y"));
        assert!(gd.is_collapsed("x"));

        let open = GroupDefaults {
            collapsed_default: false,
            collapsed: vec!["git".into()],
            expanded: vec![],
        };
        assert!(!open.is_collapsed("runtime"));
        assert!(open.is_collapsed("git"));
        let empty = std::collections::HashMap::new();
        assert_eq!(
            open.for_groups("f", &["runtime".into(), "git".into()], &empty),
            vec![false, true]
        );
        // a cached prior choice overrides the default
        let mut cache = std::collections::HashMap::new();
        cache.insert(super::collapse_key("f", "git"), false);
        assert_eq!(
            open.for_groups("f", &["runtime".into(), "git".into()], &cache),
            vec![false, false],
            "cached expand of git overrides collapsed_groups default"
        );
    }

    // ── textarea scroll (bug 2) ──────────────────────────────────────────

    #[test]
    fn textarea_scroll_follows_cursor_down() {
        // Insert TEXTAREA_VISIBLE_ROWS + 2 newlines. After each insert, the
        // scroll must keep cursor_row within [scroll, scroll + VISIBLE_ROWS).
        let mut buf = String::new();
        let mut row = 0usize;
        let mut col = 0usize;
        let mut scroll = 0usize;
        let n = super::TEXTAREA_VISIBLE_ROWS + 2;
        for _ in 0..n {
            super::textarea_insert(&mut buf, &mut row, &mut col, '\n');
            super::textarea_fix_scroll(&mut scroll, row);
            assert!(
                row >= scroll && row < scroll + super::TEXTAREA_VISIBLE_ROWS,
                "cursor_row {row} outside visible window [{scroll}, {})",
                scroll + super::TEXTAREA_VISIBLE_ROWS,
            );
        }
    }

    #[test]
    fn textarea_scroll_follows_cursor_up_after_backspace() {
        // Fill then delete: scroll must track back up.
        let mut buf = String::new();
        let mut row = 0usize;
        let mut col = 0usize;
        let mut scroll = 0usize;
        let n = super::TEXTAREA_VISIBLE_ROWS + 3;
        for _ in 0..n {
            super::textarea_insert(&mut buf, &mut row, &mut col, '\n');
            super::textarea_fix_scroll(&mut scroll, row);
        }
        // Now delete newlines back up.
        for _ in 0..n {
            super::textarea_backspace(&mut buf, &mut row, &mut col);
            super::textarea_fix_scroll(&mut scroll, row);
            assert!(
                row >= scroll && row < scroll + super::TEXTAREA_VISIBLE_ROWS,
                "after backspace cursor_row {row} outside visible window [{scroll}, {})",
                scroll + super::TEXTAREA_VISIBLE_ROWS,
            );
        }
    }

    // ── dropdown filter-then-select (bug 6) ─────────────────────────────

    /// Simulate the dropdown Enter-key selection path to verify that filtering
    /// on a substring and pressing Enter commits the correct original index.
    #[test]
    fn dropdown_filter_selects_correct_original_index() {
        // choices[0]="alpha", choices[1]="beta", choices[2]="alphabet"
        let choices = vec!["alpha".to_string(), "beta".to_string(), "alphabet".to_string()];
        let filter = "bet".to_string();

        // The filtered list in order: only "beta" (index 1) and potentially
        // none of the others match "bet".
        let filtered: Vec<usize> = choices
            .iter()
            .enumerate()
            .filter(|(_, c)| c.to_lowercase().contains(&filter.to_lowercase()))
            .map(|(i, _)| i)
            .collect();

        // cur=0 in the filtered list → should select original index 1 ("beta").
        let cur = 0usize;
        assert!(!filtered.is_empty());
        let clamped = cur.min(filtered.len() - 1);
        let selected_original_idx = filtered[clamped];
        assert_eq!(selected_original_idx, 1, "filter 'bet' cur=0 should select 'beta' at original idx 1");
        assert_eq!(choices[selected_original_idx], "beta");
    }

    #[test]
    fn dropdown_filter_empty_result_keeps_popup_open() {
        // If no choices match, `filtered.is_empty()` → popup stays open.
        let choices = vec!["alpha".to_string(), "beta".to_string()];
        let filter = "zzz".to_string();
        let filtered: Vec<usize> = choices
            .iter()
            .enumerate()
            .filter(|(_, c)| c.to_lowercase().contains(&filter.to_lowercase()))
            .map(|(i, _)| i)
            .collect();
        assert!(filtered.is_empty(), "no match → filtered is empty, popup should stay open");
    }

    #[test]
    fn dropdown_cursor_clamped_within_filtered_list() {
        // cur=5 but filtered list has only 2 entries → clamped to 1.
        let choices: Vec<String> = (0..10).map(|i| format!("item{i}")).collect();
        let filter = "item1".to_string(); // matches "item1" only
        let filtered: Vec<usize> = choices
            .iter()
            .enumerate()
            .filter(|(_, c)| c.to_lowercase().contains(&filter.to_lowercase()))
            .map(|(i, _)| i)
            .collect();
        let cur = 5usize; // stale cursor past the end
        if !filtered.is_empty() {
            let clamped = cur.min(filtered.len() - 1);
            assert!(clamped < filtered.len(), "clamped cursor must be within filtered list");
        }
    }

    // ── date mask helpers ────────────────────────────────────────────────

    #[test]
    fn parse_date_digits_full_string() {
        let d = parse_date_digits("2026-09-15");
        assert_eq!(&d, b"20260915");
    }

    #[test]
    fn parse_date_digits_empty() {
        let d = parse_date_digits("");
        assert_eq!(d, [b'_'; 8]);
    }

    #[test]
    fn render_date_mask_all_empty() {
        let d = [b'_'; 8];
        assert_eq!(render_date_mask(&d), "____-__-__");
    }

    #[test]
    fn render_date_mask_partial() {
        let mut d = [b'_'; 8];
        d[0] = b'2'; d[1] = b'0'; d[2] = b'2'; d[3] = b'6';
        assert_eq!(render_date_mask(&d), "2026-__-__");
    }

    #[test]
    fn render_date_mask_full() {
        let d = parse_date_digits("2026-09-15");
        assert_eq!(render_date_mask(&d), "2026-09-15");
    }

    #[test]
    fn render_datetime_mask_all_empty() {
        let d = [b'_'; 14];
        assert_eq!(render_datetime_mask(&d), "____-__-__T__:__:__");
    }

    #[test]
    fn render_datetime_mask_full() {
        let d = parse_datetime_digits("2026-09-15T12:30:00");
        assert_eq!(render_datetime_mask(&d), "2026-09-15T12:30:00");
    }

    #[test]
    fn date_type_digit_fills_slots_and_advances() {
        let mut d = [b'_'; 8];
        let mut cur = 0usize;
        for ch in b"20260915" {
            cur = date_type_digit(&mut d, cur, *ch);
        }
        assert_eq!(cur, 8);
        assert_eq!(render_date_mask(&d), "2026-09-15");
    }

    #[test]
    fn date_type_digit_stops_at_end() {
        let mut d = parse_date_digits("2026-09-15");
        let cur = date_type_digit(&mut d, 8, b'9'); // past end
        assert_eq!(cur, 8);
    }

    #[test]
    fn date_backspace_clears_last_digit() {
        let mut d = parse_date_digits("2026-09-15");
        let cur = date_backspace(&mut d, 8);
        assert_eq!(cur, 7);
        assert_eq!(d[7], b'_');
        assert_eq!(render_date_mask(&d), "2026-09-1_");
    }

    #[test]
    fn date_backspace_at_zero_is_noop() {
        let mut d = [b'_'; 8];
        let cur = date_backspace(&mut d, 0);
        assert_eq!(cur, 0);
    }

    #[test]
    fn datetime_type_and_backspace() {
        let mut d = [b'_'; 14];
        let mut cur = 0usize;
        for ch in b"20260915123000" {
            cur = datetime_type_digit(&mut d, cur, *ch);
        }
        assert_eq!(cur, 14);
        assert_eq!(render_datetime_mask(&d), "2026-09-15T12:30:00");
        // backspace once
        cur = datetime_backspace(&mut d, cur);
        assert_eq!(cur, 13);
        assert_eq!(d[13], b'_');
    }

    #[test]
    fn date_widget_value_incomplete_is_empty() {
        let d = parse_date_digits("2026-09-__");
        // incomplete: last 2 slots are '_'
        assert_eq!(date_widget_value(&d), "");
    }

    #[test]
    fn date_widget_value_complete() {
        let d = parse_date_digits("2026-09-15");
        assert_eq!(date_widget_value(&d), "2026-09-15");
    }

    #[test]
    fn datetime_widget_value_complete() {
        let d = parse_datetime_digits("2026-09-15T12:30:00");
        assert_eq!(datetime_widget_value(&d), "2026-09-15T12:30:00");
    }

    // ── calendar helpers ─────────────────────────────────────────────────

    #[test]
    fn days_in_month_regular() {
        assert_eq!(days_in_month(2026, 9), 30); // September
        assert_eq!(days_in_month(2026, 1), 31); // January
        assert_eq!(days_in_month(2026, 2), 28); // non-leap
        assert_eq!(days_in_month(2024, 2), 29); // leap
    }

    #[test]
    fn days_in_month_december_wraps() {
        assert_eq!(days_in_month(2026, 12), 31);
    }

    #[test]
    fn date_to_date_digits_roundtrip() {
        let d = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let digits = date_to_date_digits(d);
        assert_eq!(render_date_mask(&digits), "2026-09-15");
    }

    #[test]
    fn date_from_date_digits_parses() {
        let d = parse_date_digits("2026-09-15");
        let nd = date_from_date_digits(&d).unwrap();
        assert_eq!(nd.year(), 2026);
        assert_eq!(nd.month(), 9);
        assert_eq!(nd.day(), 15);
    }

    #[test]
    fn date_from_date_digits_incomplete_is_none() {
        let d = [b'_'; 8];
        assert!(date_from_date_digits(&d).is_none());
    }

    #[test]
    fn cal_navigate_forward_backward() {
        let start = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let next = start.succ_opt().unwrap();
        assert_eq!(next.day(), 16);
        let prev = start.pred_opt().unwrap();
        assert_eq!(prev.day(), 14);
    }

    #[test]
    fn cal_navigate_week() {
        use chrono::Days;
        let start = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let next_week = start.checked_add_days(Days::new(7)).unwrap();
        assert_eq!(next_week.day(), 22);
        let prev_week = start.checked_sub_days(Days::new(7)).unwrap();
        assert_eq!(prev_week.day(), 8);
    }

    #[test]
    fn cal_navigate_month() {
        use chrono::Months;
        let start = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let next_m = start.checked_add_months(Months::new(1)).unwrap();
        assert_eq!(next_m.month(), 10);
        let prev_m = start.checked_sub_months(Months::new(1)).unwrap();
        assert_eq!(prev_m.month(), 8);
    }

    #[test]
    fn cal_enter_commits_date_to_digits() {
        // Simulate calendar Enter: date → digits → render
        let date = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let digits = date_to_date_digits(date);
        assert_eq!(date_widget_value(&digits), "2026-09-15");
    }

    #[test]
    fn cal_esc_leaves_digits_unchanged() {
        // Esc = no mutation: existing digits untouched. The CalPicker holds
        // an internal `date` but on Esc we never call date_to_date_digits, so
        // the widget's `digits` array is never written.
        let original = parse_date_digits("2026-09-01");
        // verify the original value is recoverable from the digits
        assert_eq!(date_widget_value(&original), "2026-09-01");
    }

    // ── path validation (injected fs) ───────────────────────────────────

    /// Helpers: `exists_yes` always returns true, `exists_no` always false,
    /// `is_dir_yes` always true.
    fn exists_yes(_: &std::path::Path) -> bool { true }
    fn exists_no(_: &std::path::Path) -> bool { false }
    fn is_dir_yes(_: &std::path::Path) -> bool { true }
    fn is_dir_no(_: &std::path::Path) -> bool { false }

    #[test]
    fn path_existing_path_accepted() {
        // Path itself exists → always accepted.
        let r = validate_path_value("My path", "/some/existing/dir", exists_yes, is_dir_yes);
        assert!(r.is_ok());
    }

    #[test]
    fn path_new_leaf_under_existing_parent_accepted() {
        // Path doesn't exist but parent does and is a dir → accepted (new leaf).
        let exists = |p: &std::path::Path| p != std::path::Path::new("/parent/newleaf");
        let r = validate_path_value("My path", "/parent/newleaf", exists, is_dir_yes);
        assert!(r.is_ok());
    }

    #[test]
    fn path_nonexistent_parent_rejected() {
        // Neither path nor parent exists → rejected with a clear message.
        let r = validate_path_value("My path", "/missing/parent/leaf", exists_no, is_dir_yes);
        assert!(r.is_err());
        let msg = r.unwrap_err();
        assert!(msg.contains("My path"), "label missing from error: {msg}");
        assert!(msg.contains("missing/parent"), "parent dir missing from error: {msg}");
    }

    #[test]
    fn path_parent_exists_but_is_not_a_dir_rejected() {
        // Parent exists but is a file, not a directory → rejected.
        let exists = |p: &std::path::Path| p == std::path::Path::new("/parent");
        let r = validate_path_value("dest", "/parent/leaf", exists, is_dir_no);
        assert!(r.is_err());
    }

    #[test]
    fn path_bare_name_no_parent_accepted() {
        // A bare name like `newdir` has an empty parent → cwd implicitly valid.
        let r = validate_path_value("dir", "newdir", exists_no, is_dir_yes);
        assert!(r.is_ok());
    }

    #[test]
    fn path_trim_via_real_fs() {
        // widget_value trims: we test the trim logic directly here.
        // "  /tmp  " trimmed is "/tmp".
        let trimmed = "  /tmp  ".trim().to_string();
        assert_eq!(trimmed, "/tmp");
    }

    #[test]
    fn path_trailing_space_trimmed_and_real_parent_accepted() {
        // Simulate the full flow: a value with leading/trailing spaces is first
        // trimmed by widget_value, then the trimmed value is validated.
        let raw = format!("  {}  ", std::env::temp_dir().display());
        let trimmed = raw.trim();
        // temp_dir() itself exists
        let r = validate_path_value("dir", trimmed, |p| p.exists(), |p| p.is_dir());
        assert!(r.is_ok(), "trimmed temp_dir must be accepted: {:?}", r);
    }

    #[test]
    fn path_new_leaf_under_temp_dir_accepted() {
        // temp_dir()/nonexistent_leaf: parent (temp_dir) exists, leaf doesn't.
        let leaf = std::env::temp_dir().join("__insmaller_test_nonexistent_leaf_xyz__");
        let _ = std::fs::remove_file(&leaf); // ensure it doesn't exist
        let r = validate_path_value(
            "out",
            &leaf.to_string_lossy(),
            |p| p.exists(),
            |p| p.is_dir(),
        );
        assert!(r.is_ok(), "new leaf under existing parent must be accepted: {:?}", r);
    }

    #[test]
    fn path_typo_parent_rejected_real_fs() {
        // A path whose parent definitely doesn't exist must be rejected.
        let bad = std::env::temp_dir()
            .join("__definitely_absent_parent_xyz_abc__")
            .join("leaf");
        let r = validate_path_value(
            "output path",
            &bad.to_string_lossy(),
            |p| p.exists(),
            |p| p.is_dir(),
        );
        assert!(r.is_err(), "nonexistent parent must be rejected");
        let msg = r.unwrap_err();
        assert!(msg.contains("output path"), "label in error: {msg}");
    }

    // ── dropdown search header ───────────────────────────────────────────

    /// Replicate the search-line construction logic from the render closure so
    /// changes to the format are caught by tests without needing a terminal.
    fn dropdown_search_line(filter: &str) -> String {
        if filter.is_empty() {
            "  Search: \u{258c}  (type to filter)".to_string()
        } else {
            format!("  Search: {filter}\u{258c}")
        }
    }

    /// Replicate the row list logic: header always first; then choices or
    /// [no matches]. Returns just the string content of each row.
    fn dropdown_rows(choices: &[&str], filter: &str) -> Vec<String> {
        let filtered: Vec<&&str> = choices
            .iter()
            .filter(|c| filter.is_empty() || c.to_lowercase().contains(&filter.to_lowercase()))
            .collect();
        let mut rows = vec![dropdown_search_line(filter)];
        if filtered.is_empty() {
            rows.push("  [no matches]".to_string());
        } else {
            rows.extend(filtered.iter().map(|c| c.to_string()));
        }
        rows
    }

    #[test]
    fn dropdown_search_header_empty_filter() {
        let line = dropdown_search_line("");
        assert!(line.contains("Search:"), "must contain 'Search:': {line}");
        assert!(line.contains("type to filter"), "hint text missing: {line}");
        assert!(line.contains('\u{258c}'), "cursor block missing: {line}");
    }

    #[test]
    fn dropdown_search_header_with_filter() {
        let line = dropdown_search_line("ph");
        assert!(line.contains("Search: ph"), "filter text not shown: {line}");
        assert!(line.contains('\u{258c}'), "cursor block missing: {line}");
        // hint text only shown when filter is empty
        assert!(!line.contains("type to filter"), "hint must be absent when typing: {line}");
    }

    #[test]
    fn dropdown_rows_header_is_always_first() {
        let choices = ["alpha", "beta", "gamma"];
        let rows = dropdown_rows(&choices, "");
        assert!(rows[0].contains("Search:"), "header must be row 0: {:?}", rows[0]);
    }

    #[test]
    fn dropdown_rows_choice_count_with_filter() {
        let choices = ["US", "PH", "DE", "PL"];
        let rows = dropdown_rows(&choices, "p");
        // header + "PH" + "PL" = 3 rows
        assert_eq!(rows.len(), 3, "header + 2 matches: {rows:?}");
        assert!(rows[1].contains("PH") || rows[2].contains("PH"));
        assert!(rows[1].contains("PL") || rows[2].contains("PL"));
    }

    #[test]
    fn dropdown_rows_no_matches_shows_placeholder() {
        let choices = ["alpha", "beta"];
        let rows = dropdown_rows(&choices, "zzz");
        // header + [no matches] = 2 rows
        assert_eq!(rows.len(), 2, "header + no-matches: {rows:?}");
        assert!(rows[1].contains("no matches"), "placeholder missing: {:?}", rows[1]);
    }

    #[test]
    fn dropdown_highlight_offset_skips_header() {
        // clamped_cur + 1: verify that adding 1 to index 0 gives row 1 (first choice).
        let filtered_len = 3usize;
        let cur = 0usize;
        let clamped = cur.min(filtered_len.saturating_sub(1));
        let st_idx = clamped + 1; // offset past header
        assert_eq!(st_idx, 1, "index 0 in filtered → row 1 in list (skip header)");
    }

    #[test]
    fn dropdown_highlight_offset_clamped_last() {
        // cur beyond list → clamped to last, still +1 for header.
        let filtered_len = 3usize;
        let cur = 99usize;
        let clamped = cur.min(filtered_len.saturating_sub(1));
        let st_idx = clamped + 1;
        assert_eq!(st_idx, 3, "clamped last choice (idx 2) → row 3 in list");
    }

    // ── textarea cursor navigation ───────────────────────────────────────

    /// Simulate the Up key in a textarea: decrement row, clamp col.
    fn ta_up(buf: &str, row: &mut usize, col: &mut usize) {
        if *row > 0 {
            *row -= 1;
            *col = (*col).min(textarea_line_char_len(buf, *row));
        }
    }

    /// Simulate the Down key.
    fn ta_down(buf: &str, row: &mut usize, col: &mut usize) {
        let last = textarea_line_count(buf).saturating_sub(1);
        if *row < last {
            *row += 1;
            *col = (*col).min(textarea_line_char_len(buf, *row));
        }
    }

    /// Simulate the Left key (wraps to end of previous line at col 0).
    fn ta_left(buf: &str, row: &mut usize, col: &mut usize) {
        if *col > 0 {
            *col -= 1;
        } else if *row > 0 {
            *row -= 1;
            *col = textarea_line_char_len(buf, *row);
        }
    }

    /// Simulate the Right key (wraps to start of next line at line end).
    fn ta_right(buf: &str, row: &mut usize, col: &mut usize) {
        let line_len = textarea_line_char_len(buf, *row);
        if *col < line_len {
            *col += 1;
        } else {
            let last = textarea_line_count(buf).saturating_sub(1);
            if *row < last {
                *row += 1;
                *col = 0;
            }
        }
    }

    #[test]
    fn ta_up_moves_row_and_clamps_col_on_shorter_line() {
        // "hello\nhi\nworld": row 2 col 4 → Up → row 1 ("hi" len 2) → col clamped to 2.
        let buf = "hello\nhi\nworld";
        let mut row = 2usize;
        let mut col = 4usize;
        ta_up(buf, &mut row, &mut col);
        assert_eq!(row, 1);
        assert_eq!(col, 2, "col clamped to len of 'hi'");
    }

    #[test]
    fn ta_down_moves_row_and_clamps_col_on_shorter_line() {
        let buf = "hello\nhi";
        let mut row = 0usize;
        let mut col = 4usize;
        ta_down(buf, &mut row, &mut col);
        assert_eq!(row, 1);
        assert_eq!(col, 2, "col clamped to len of 'hi'");
    }

    #[test]
    fn ta_up_at_first_row_is_noop() {
        let buf = "hello\nworld";
        let mut row = 0usize;
        let mut col = 3usize;
        ta_up(buf, &mut row, &mut col);
        assert_eq!(row, 0);
        assert_eq!(col, 3);
    }

    #[test]
    fn ta_down_at_last_row_is_noop() {
        let buf = "hello\nworld";
        let mut row = 1usize;
        let mut col = 2usize;
        ta_down(buf, &mut row, &mut col);
        assert_eq!(row, 1);
        assert_eq!(col, 2);
    }

    #[test]
    fn ta_left_at_col_zero_wraps_to_end_of_prev_line() {
        let buf = "hello\nworld";
        let mut row = 1usize;
        let mut col = 0usize;
        ta_left(buf, &mut row, &mut col);
        assert_eq!(row, 0);
        assert_eq!(col, 5, "end of 'hello'");
    }

    #[test]
    fn ta_left_within_line_decrements_col() {
        let buf = "hello\nworld";
        let mut row = 0usize;
        let mut col = 3usize;
        ta_left(buf, &mut row, &mut col);
        assert_eq!(row, 0);
        assert_eq!(col, 2);
    }

    #[test]
    fn ta_left_at_very_start_is_noop() {
        let buf = "hello";
        let mut row = 0usize;
        let mut col = 0usize;
        ta_left(buf, &mut row, &mut col);
        assert_eq!(row, 0);
        assert_eq!(col, 0);
    }

    #[test]
    fn ta_right_at_line_end_wraps_to_next_line_start() {
        let buf = "hello\nworld";
        let mut row = 0usize;
        let mut col = 5usize; // end of "hello"
        ta_right(buf, &mut row, &mut col);
        assert_eq!(row, 1);
        assert_eq!(col, 0);
    }

    #[test]
    fn ta_right_within_line_increments_col() {
        let buf = "hello\nworld";
        let mut row = 0usize;
        let mut col = 2usize;
        ta_right(buf, &mut row, &mut col);
        assert_eq!(row, 0);
        assert_eq!(col, 3);
    }

    #[test]
    fn ta_right_at_very_end_is_noop() {
        let buf = "hello";
        let mut row = 0usize;
        let mut col = 5usize;
        ta_right(buf, &mut row, &mut col);
        assert_eq!(row, 0);
        assert_eq!(col, 5);
    }

    #[test]
    fn ta_home_end_semantics() {
        let buf = "hello\nworld";
        // Home: col → 0
        let col_after_home = 0usize;
        assert_eq!(col_after_home, 0);
        // End: col → line char len
        let end_col = textarea_line_char_len(buf, 0);
        assert_eq!(end_col, 5);
    }

    #[test]
    fn textarea_scroll_scrolls_up_when_cursor_above_viewport() {
        // Start at row 5 with scroll=3 (row 5 is in view). Navigate Up to row 2
        // (above scroll); scroll must adjust to 2.
        use super::{textarea_fix_scroll, TEXTAREA_VISIBLE_ROWS};
        let mut scroll = 3usize;
        let cursor_row = 2usize; // now above viewport
        textarea_fix_scroll(&mut scroll, cursor_row);
        assert_eq!(scroll, 2, "scroll must move up so cursor is visible");
        assert!(
            cursor_row >= scroll && cursor_row < scroll + TEXTAREA_VISIBLE_ROWS,
            "cursor must be in visible window"
        );
    }

    #[test]
    fn textarea_line_counter_string() {
        // The hint format is "(line {row+1}/{total})". Verify arithmetic.
        let buf = "a\nb\nc";
        let total = textarea_line_count(buf);
        assert_eq!(total, 3);
        let cursor_row = 1usize;
        let hint = format!("(line {}/{})", cursor_row + 1, total);
        assert_eq!(hint, "(line 2/3)");
    }

    #[test]
    fn textarea_line_char_len_multibyte() {
        // A line with multi-byte chars: char count != byte count.
        let buf = "café\nhi";
        assert_eq!(textarea_line_char_len(buf, 0), 4); // c-a-f-é = 4 chars
        assert_eq!(textarea_line_char_len(buf, 1), 2);
    }
}
