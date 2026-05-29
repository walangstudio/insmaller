//! TTY-aware `InputResolver` for the CLI binary. Layers in front of a
//! fallback (always `EnvResolver` in prod) so `prompt`/`input` steps in a
//! task can read stdin on an attached terminal — masking the value for
//! `secret = true` — while non-interactive runs keep the env-only contract
//! that makes them structurally non-blocking. The TTY check + environment
//! lookup + line read are pushed behind a small `InteractiveIo` trait so
//! the resolver is unit-testable without a real terminal.

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use insmaller_core::{InputResolver, PromptSpec, ResolvedInput};
use std::io::{IsTerminal, Write};
use std::sync::Mutex;

/// Process-global serializer for interactive reads. crossterm's raw mode is
/// a process resource; two parallel tasks each entering a `prompt` step
/// must not race `enable_raw_mode`/`event::read`/`disable_raw_mode` against
/// each other or interleave keystrokes between two `buf`s. Lock duration is
/// bounded by the user's typing speed — short enough that a sync `Mutex`
/// (no async holding) is the right primitive.
static INTERACTIVE_LOCK: Mutex<()> = Mutex::new(());

/// Outcome of an interactive read.
pub enum InteractiveLine {
    /// A line was entered (possibly empty).
    Line(String),
    /// The user cancelled (Ctrl+C / Ctrl+D / Esc).
    Cancel,
    /// stdin is not a TTY — caller should defer to the fallback resolver.
    NoTty,
}

/// Injectable I/O surface — production uses the real terminal, tests pass a
/// fake so they don't need a PTY.
pub trait InteractiveIo: Send + Sync {
    /// True when interactive prompting is safe. Requires BOTH stdin (to read
    /// the user's value) AND stdout (where the prompt is written) to be
    /// terminals — stdout-redirected runs (`> log`) must defer to the env
    /// fallback so the user doesn't type blind.
    fn is_tty(&self) -> bool;
    /// Process env lookup (resolver path 1: env always wins on hit).
    fn env(&self, key: &str) -> Option<String>;
    /// Display `message` and read a line. `secret = true` ⇒ mask with `*`.
    fn read_line(&self, message: &str, secret: bool) -> std::io::Result<InteractiveLine>;
}

/// Production I/O: `std::io::stdin().is_terminal()` + `std::io::stdout().is_terminal()`,
/// `std::env::var`, plus a crossterm-driven masked line reader for secret prompts.
pub struct RealIo;

impl InteractiveIo for RealIo {
    fn is_tty(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    }
    fn env(&self, key: &str) -> Option<String> {
        // Shared with EnvResolver so the empty-is-absent rule has one source.
        insmaller_core::env_nonempty(key)
    }
    fn read_line(&self, message: &str, secret: bool) -> std::io::Result<InteractiveLine> {
        if !self.is_tty() {
            return Ok(InteractiveLine::NoTty);
        }
        // The lock-wait and the human-speed read both block. `resolve()` is a
        // sync trait method called from inside an async step on a tokio worker
        // thread (PromptProcessor::run), so a naked blocking read parks a
        // worker — starving step-timeout timers and any parallel task in the
        // same wave. Run the whole critical section under `block_in_place` so
        // tokio can move other tasks off this worker while we wait on the lock
        // and on the user. (block_in_place panics on a current-thread runtime
        // or off-runtime, hence the guard; tests drive FakeIo, not RealIo.)
        maybe_block_in_place(|| self.read_line_blocking(message, secret))
    }
}

impl RealIo {
    /// The actual blocking read, factored out so `read_line` can wrap it in
    /// `block_in_place`. Holds `INTERACTIVE_LOCK` for the full duration so two
    /// concurrent prompts on the one shared terminal serialize.
    fn read_line_blocking(&self, message: &str, secret: bool) -> std::io::Result<InteractiveLine> {
        // Poison is recoverable: we don't care about a prior holder's state.
        let _guard = INTERACTIVE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut out = std::io::stdout();
        // Render the prompt before either path (raw mode silences echo, so
        // the prompt must precede the mode switch for the non-secret case to
        // look sane after rendering).
        write!(out, "{message} ")?;
        out.flush()?;
        if secret {
            read_masked_line()
        } else {
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            let trimmed = s.trim_end_matches(['\r', '\n']).to_string();
            Ok(InteractiveLine::Line(trimmed))
        }
    }
}

/// Run `f` under `tokio::task::block_in_place` when on a multi-thread runtime
/// (lets the scheduler relocate other tasks while this worker blocks); run it
/// directly otherwise. `block_in_place` panics off-runtime and on a
/// current-thread runtime, so both are checked first.
fn maybe_block_in_place<T>(f: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// RAII for crossterm raw mode. Drop runs on every unwind path (`?`-return,
/// panic, early `return`) — the inline-disable pattern this replaces only
/// caught `?` and leaked the terminal on panic. Mirrors `tui.rs::TermGuard`.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> std::io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// RAII for crossterm bracketed paste mode. When enabled, terminals send
/// pasted text as a single `Event::Paste(String)` instead of synthesizing
/// per-character key events — lets a pasted secret arrive atomically
/// instead of leaking the second line into the next prompt.
struct BracketedPasteGuard;

impl BracketedPasteGuard {
    fn enable() -> std::io::Result<Self> {
        crossterm::execute!(std::io::stdout(), EnableBracketedPaste)?;
        Ok(Self)
    }
}

impl Drop for BracketedPasteGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    }
}

/// Read a line in raw mode, echoing `*` per character. Backspace pops a char
/// (and the `*`); Enter ends the line; Ctrl+C / Ctrl+D / Esc cancels. Other
/// Ctrl+letter chords are silently dropped (never pushed as literals). On
/// terminals supporting bracketed paste, a pasted payload arrives atomically
/// via `Event::Paste` and is appended in one shot (no leakage of trailing
/// lines into the next read). KeyEventKind is filtered to Press|Repeat so
/// Windows legacy console — which emits both Press and Release — doesn't
/// double-count keystrokes.
fn read_masked_line() -> std::io::Result<InteractiveLine> {
    let _raw = RawModeGuard::enable()?;
    // LOAD-BEARING: `_paste_guard` must live until the end of this function.
    // Its Drop emits DisableBracketedPaste; dropping it early (e.g. rewriting
    // to `let _ = …` or deleting the "unused" binding) turns paste mode off
    // before the read loop and reintroduces the multi-line-paste leak. The
    // `.ok()` is deliberate: a terminal without bracketed-paste support just
    // falls back to per-key events, which the loop still handles correctly.
    let _paste_guard = BracketedPasteGuard::enable().ok();
    let mut buf = String::new();
    let mut out = std::io::stdout();
    loop {
        match crossterm::event::read()? {
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            }) => match code {
                KeyCode::Enter => {
                    writeln!(out)?;
                    out.flush()?;
                    return Ok(InteractiveLine::Line(buf));
                }
                KeyCode::Esc => {
                    writeln!(out)?;
                    out.flush()?;
                    return Ok(InteractiveLine::Cancel);
                }
                // Ctrl+C and Ctrl+D both cancel (Ctrl+D matches POSIX `read`
                // EOF semantics so users don't accidentally type 'd' into a
                // password when reaching for the standard EOF shortcut).
                KeyCode::Char('c' | 'd') if modifiers.contains(KeyModifiers::CONTROL) => {
                    writeln!(out)?;
                    out.flush()?;
                    return Ok(InteractiveLine::Cancel);
                }
                // Any other Ctrl+letter chord is dropped, never pushed as a
                // literal — otherwise Ctrl+U / Ctrl+W / Ctrl+L would silently
                // corrupt the captured secret with control bytes the user
                // can't see.
                KeyCode::Char(_) if modifiers.contains(KeyModifiers::CONTROL) => {}
                // Match-guard form so the empty-buf case falls through
                // silently (clippy::collapsible_match — pop() mutates either
                // way, but on empty buf there's nothing to un-render).
                KeyCode::Backspace if buf.pop().is_some() => {
                    write!(out, "\x08 \x08")?;
                    out.flush()?;
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    write!(out, "*")?;
                    out.flush()?;
                }
                _ => {}
            },
            Event::Paste(s) => {
                // Collapse a multi-line paste onto one line by dropping ONLY
                // newlines/carriage-returns — keep every other char (incl.
                // tabs and other control bytes) verbatim so a pasted secret
                // isn't silently mutated. Masking hides corruption, so the
                // captured value must equal the source minus line breaks.
                for c in s.chars().filter(|&c| c != '\n' && c != '\r') {
                    buf.push(c);
                    write!(out, "*")?;
                }
                out.flush()?;
            }
            _ => {}
        }
    }
}

/// Layers `InteractiveIo` over a fallback resolver. Order: env hit → fallback
/// (preserves automation when a TTY happens to be attached); else TTY prompt;
/// else fallback again (non-TTY hands off without ever touching stdin).
pub struct InteractiveResolver {
    io: Box<dyn InteractiveIo>,
    fallback: Box<dyn InputResolver>,
}

impl InteractiveResolver {
    pub fn new<I, F>(io: I, fallback: F) -> Self
    where
        I: InteractiveIo + 'static,
        F: InputResolver + 'static,
    {
        Self {
            io: Box::new(io),
            fallback: Box::new(fallback),
        }
    }
}

impl InputResolver for InteractiveResolver {
    fn resolve(&self, key: &str, spec: &PromptSpec) -> ResolvedInput {
        // Env always wins — keeps the existing `VAR=value insmaller …`
        // automation working even when stdin happens to be a TTY.
        if let Some(v) = self.io.env(&spec.env_key) {
            return ResolvedInput::Value(v);
        }
        if !self.io.is_tty() {
            return self.fallback.resolve(key, spec);
        }
        match self.io.read_line(&spec.message, spec.secret) {
            Ok(InteractiveLine::Line(v)) => {
                if v.is_empty() {
                    if spec.required {
                        ResolvedInput::Fail(format!("input '{}' required", spec.env_key))
                    } else {
                        ResolvedInput::Skip
                    }
                } else {
                    ResolvedInput::Value(v)
                }
            }
            // Cancel on an optional input is treated as Skip (matches the
            // env-only path's behavior for an absent optional), so an Esc
            // doesn't abort an entire task over a discretionary prompt.
            Ok(InteractiveLine::Cancel) => {
                if spec.required {
                    ResolvedInput::Fail(format!("input '{}' cancelled", spec.env_key))
                } else {
                    ResolvedInput::Skip
                }
            }
            // A read race that surfaces a non-TTY (e.g. stdin was redirected
            // mid-flight) defers to the fallback rather than failing loudly.
            Ok(InteractiveLine::NoTty) => self.fallback.resolve(key, spec),
            Err(e) => ResolvedInput::Fail(format!("input '{}' read error: {e}", spec.env_key)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insmaller_core::EnvResolver;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Test double: scripted answers + fake env + scripted tty flag.
    struct FakeIo {
        tty: bool,
        env: HashMap<String, String>,
        answers: Mutex<Vec<InteractiveLine>>,
    }

    impl FakeIo {
        fn new(tty: bool) -> Self {
            Self {
                tty,
                env: HashMap::new(),
                answers: Mutex::new(Vec::new()),
            }
        }
        fn with_env(mut self, k: &str, v: &str) -> Self {
            self.env.insert(k.into(), v.into());
            self
        }
        fn queue(self, line: InteractiveLine) -> Self {
            self.answers.lock().unwrap().insert(0, line);
            self
        }
    }

    impl InteractiveIo for FakeIo {
        fn is_tty(&self) -> bool {
            self.tty
        }
        fn env(&self, key: &str) -> Option<String> {
            self.env.get(key).cloned()
        }
        fn read_line(&self, _message: &str, _secret: bool) -> std::io::Result<InteractiveLine> {
            Ok(self
                .answers
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(InteractiveLine::NoTty))
        }
    }

    fn spec(env_key: &str, required: bool, secret: bool) -> PromptSpec {
        PromptSpec {
            env_key: env_key.into(),
            message: format!("{env_key}:"),
            required,
            secret,
        }
    }

    /// Env key unlikely to be set in any host running the test suite.
    const UNSET: &str = "INSMALLER_INTERACTIVE_TEST_NEVER_SET_XYZ";

    #[test]
    fn env_wins_even_on_tty_and_skips_prompt() {
        let io = FakeIo::new(true).with_env("TOKEN", "abc");
        let r = InteractiveResolver::new(io, EnvResolver);
        let out = r.resolve("TOKEN", &spec("TOKEN", true, false));
        assert_eq!(out, ResolvedInput::Value("abc".into()));
    }

    #[test]
    fn no_tty_delegates_to_fallback() {
        // Fallback = EnvResolver; spec required + no env → Fail (env contract).
        let io = FakeIo::new(false);
        let r = InteractiveResolver::new(io, EnvResolver);
        let out = r.resolve("K", &spec(UNSET, true, false));
        assert!(matches!(out, ResolvedInput::Fail(_)));
    }

    #[test]
    fn no_tty_optional_missing_skips_via_fallback() {
        let io = FakeIo::new(false);
        let r = InteractiveResolver::new(io, EnvResolver);
        let out = r.resolve("K", &spec(UNSET, false, false));
        assert_eq!(out, ResolvedInput::Skip);
    }

    #[test]
    fn tty_prompt_reads_value() {
        let io = FakeIo::new(true).queue(InteractiveLine::Line("typed".into()));
        let r = InteractiveResolver::new(io, EnvResolver);
        let out = r.resolve("X", &spec("X", true, false));
        assert_eq!(out, ResolvedInput::Value("typed".into()));
    }

    #[test]
    fn tty_prompt_empty_required_fails_fast() {
        let io = FakeIo::new(true).queue(InteractiveLine::Line(String::new()));
        let r = InteractiveResolver::new(io, EnvResolver);
        let out = r.resolve("X", &spec("X", true, false));
        assert!(matches!(out, ResolvedInput::Fail(_)));
    }

    #[test]
    fn tty_prompt_cancel_required_reports_cancelled() {
        let io = FakeIo::new(true).queue(InteractiveLine::Cancel);
        let r = InteractiveResolver::new(io, EnvResolver);
        let out = r.resolve("X", &spec("X", true, false));
        match out {
            ResolvedInput::Fail(m) => assert!(m.contains("cancelled")),
            o => panic!("expected Fail(cancelled), got {o:?}"),
        }
    }

    #[test]
    fn tty_prompt_cancel_optional_skips() {
        // Esc/Ctrl+C on a `required=false` prompt becomes Skip — matches the
        // env-only path so cancelling a discretionary prompt doesn't abort
        // the task.
        let io = FakeIo::new(true).queue(InteractiveLine::Cancel);
        let r = InteractiveResolver::new(io, EnvResolver);
        let out = r.resolve("X", &spec("X", false, false));
        assert_eq!(out, ResolvedInput::Skip);
    }

    #[test]
    fn tty_prompt_optional_empty_skips() {
        let io = FakeIo::new(true).queue(InteractiveLine::Line(String::new()));
        let r = InteractiveResolver::new(io, EnvResolver);
        let out = r.resolve("X", &spec("X", false, false));
        assert_eq!(out, ResolvedInput::Skip);
    }
}
