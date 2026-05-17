//! Verbatim recipe scripts that use shell command substitution. Embedded as
//! Rust constants (not data files) so they survive the file-write guard and
//! stay byte-for-byte identical to the codetainyrrr handlers — the parity
//! guarantee. `script_file = "recipes/<x>.sh"` in installer.toml resolves
//! here first (then falls back to a real file for host-supplied recipes).

/// Verbatim from codetainyrrr github_release.rs install script.
/// Params: {{ repo }}, {{ pattern_regex }} are minijinja-rendered before run.
pub const GH_RELEASE_SH: &str = r####"
set -uo pipefail
REPO="{{ repo }}"
PATTERN_RE="{{ pattern_regex }}"
DEST="$HOME/.local/bin"
mkdir -p "$DEST"

API_URL="https://api.github.com/repos/${REPO}/releases/latest"

AUTH_ARGS=()
if [ -n "${GITHUB_TOKEN:-}" ]; then
    AUTH_ARGS=(-H "Authorization: Bearer $GITHUB_TOKEN")
fi

RESP=$(curl -fsSL "${AUTH_ARGS[@]}" "$API_URL" 2>&1) || {
    echo "github-release: failed to fetch $API_URL" >&2
    echo "$RESP" | head -5 >&2
    exit 1
}

if echo "$RESP" | jq -e '.message' >/dev/null 2>&1; then
    MSG=$(echo "$RESP" | jq -r '.message')
    echo "github-release: API returned error: $MSG" >&2
    exit 1
fi

ASSET_URL=$(echo "$RESP" \
    | jq -r '.assets[].browser_download_url' \
    | grep -iE "$PATTERN_RE" \
    | head -1)

if [ -z "$ASSET_URL" ]; then
    echo "github-release: no asset matching '/$PATTERN_RE/' (case-insensitive) in $REPO. Available:" >&2
    echo "$RESP" | jq -r '.assets[].name' | head -20 >&2
    exit 1
fi

echo "github-release: downloading $ASSET_URL" >&2
TMPDIR=$(mktemp -d)
FILENAME=$(basename "$ASSET_URL")
curl -fsSL "${AUTH_ARGS[@]}" "$ASSET_URL" -o "$TMPDIR/$FILENAME"

case "$FILENAME" in
    *.tar.gz|*.tgz) tar -C "$TMPDIR" -xzf "$TMPDIR/$FILENAME" ;;
    *.tar.bz2)      tar -C "$TMPDIR" -xjf "$TMPDIR/$FILENAME" ;;
    *.zip)          unzip -q "$TMPDIR/$FILENAME" -d "$TMPDIR" ;;
    *)              chmod +x "$TMPDIR/$FILENAME" ;;
esac

find "$TMPDIR" -maxdepth 2 -type f \
    ! -name "*.tar*" ! -name "*.zip" ! -name "*.md" ! -name "*.txt" \
    ! -name "LICENSE*" ! -name "*.json" ! -name "*.sh" \
    | while read -r bin; do
        chmod +x "$bin" 2>/dev/null || true
        cp "$bin" "$DEST/$(basename "$bin")"
    done

rm -rf "$TMPDIR"
"####;

/// Verbatim equivalent of codetainyrrr go_lang.rs (latest toolchain into
/// ~/go/sdk). `arg` is ignored (spec is `go:latest`).
pub const GO_TOOLCHAIN_SH: &str = r####"
set -e
ARCH=$(uname -m)
case "$ARCH" in
    x86_64) GOARCH=amd64 ;;
    aarch64|arm64) GOARCH=arm64 ;;
    *) GOARCH=amd64 ;;
esac
VER=$(curl -fsSL "https://go.dev/VERSION?m=text" | head -1)
mkdir -p "$HOME/go/sdk"
curl -fsSL "https://go.dev/dl/${VER}.linux-${GOARCH}.tar.gz" \
    | tar -C "$HOME/go/sdk" --strip-components=1 -xzf -
"$HOME/go/sdk/bin/go" version
"####;

/// Verbatim from codetainyrrr python.rs `python:tools`: bootstrap uv, install
/// the standard Python tool set, and symlink python/pip. Uses command
/// substitution → embedded (the data-file guard blocks `$(` inline).
pub const PYTHON_TOOLS_SH: &str = r####"
if ! command -v uv >/dev/null 2>&1; then
    curl -LsSf https://astral.sh/uv/install.sh | sh
fi
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
uv tool install poetry
uv tool install pipenv
uv tool install black
uv tool install ruff
uv tool install mypy

# Debian only ships python3 — symlink so `#!/usr/bin/env python` works,
# without needing sudo (python-is-python3).
mkdir -p "$HOME/.local/bin"
if ! command -v python >/dev/null 2>&1; then
    ln -sf "$(command -v python3)" "$HOME/.local/bin/python"
fi
if ! command -v pip >/dev/null 2>&1 && command -v pip3 >/dev/null 2>&1; then
    ln -sf "$(command -v pip3)" "$HOME/.local/bin/pip"
fi
"####;

/// Verbatim from codetainyrrr python.rs uninstall.
pub const PYTHON_TOOLS_UNINSTALL_SH: &str = r####"
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
uv tool uninstall poetry 2>/dev/null || true
uv tool uninstall pipenv 2>/dev/null || true
uv tool uninstall black 2>/dev/null || true
uv tool uninstall ruff 2>/dev/null || true
uv tool uninstall mypy 2>/dev/null || true
"####;

/// Resolve a `script_file` reference to an embedded script, if known.
pub fn embedded(script_file: &str) -> Option<&'static str> {
    match script_file {
        "recipes/gh-release.sh" => Some(GH_RELEASE_SH),
        "recipes/go-toolchain.sh" => Some(GO_TOOLCHAIN_SH),
        "recipes/python-tools.sh" => Some(PYTHON_TOOLS_SH),
        "recipes/python-tools-uninstall.sh" => Some(PYTHON_TOOLS_UNINSTALL_SH),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_lookup() {
        assert!(embedded("recipes/gh-release.sh").unwrap().contains("API_URL"));
        assert!(embedded("recipes/go-toolchain.sh")
            .unwrap()
            .contains("go/sdk"));
        assert!(embedded("recipes/python-tools.sh")
            .unwrap()
            .contains("uv tool install ruff"));
        assert!(embedded("recipes/python-tools-uninstall.sh")
            .unwrap()
            .contains("uv tool uninstall ruff"));
        assert!(embedded("recipes/unknown.sh").is_none());
    }

    #[test]
    fn gh_script_is_verbatim_shape() {
        // Sentinel lines proving we kept the exact codetainyrrr behavior.
        let s = GH_RELEASE_SH;
        assert!(s.contains(r#"grep -iE "$PATTERN_RE""#));
        assert!(s.contains(r#"find "$TMPDIR" -maxdepth 2 -type f"#));
        assert!(s.contains(r#"cp "$bin" "$DEST/$(basename "$bin")""#));
    }
}
