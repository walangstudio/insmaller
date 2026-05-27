# Antivirus false positives

insmaller is an unsigned installer that downloads and runs scripts. That
behavior pattern — fetch payload, write to PATH dirs, spawn a shell — is what
droppers do, so heuristic AV and Windows SmartScreen sometimes flag it. The
binary is clean; this documents why it happens and what reduces it.

## What the project already does

- **Embedded version metadata + `asInvoker` manifest** (`crates/insmaller-cli/build.rs`).
  A blank version resource is a heuristic penalty; the manifest also opts the
  exe out of Windows "installer detection," which otherwise forces a UAC prompt
  on unsigned binaries whose name/contents look installer-ish.
- **No packer.** The release profile strips symbols and uses LTO, but the binary
  is never UPX-packed. Packing is the single biggest false-positive trigger.
- **Published `SHA256SUMS`** with every GitHub release so users can verify the
  artifact they downloaded matches what CI built.
- **`require_sha256_for_exec`** (engine setting) makes `download` steps that
  fetch an executable mandate a checksum — fewer unverified executables on disk.
- **`setup_writes_config_only`** (engine setting) lets a consumer keep all
  install scripts inside their container/target so the host runs none.

## What needs a maintainer with a certificate

- **Authenticode code signing** is the highest-impact fix and the only one that
  builds SmartScreen reputation. The release workflow has an inert signing step
  (`.github/workflows/release.yml`, "Sign (Windows)"), gated on the repo
  variable `SIGN_WINDOWS=true`. To enable it:
  - Get a certificate: **Azure Trusted Signing** (~$10/mo, Microsoft-backed, no
    HSM) or a standard OV/EV code-signing cert.
  - Add the signing secrets and wire `signtool` (or `azure/trusted-signing-action`)
    into that step, then set `SIGN_WINDOWS=true`.

## When a vendor still flags a release

1. Confirm the artifact's SHA-256 matches `SHA256SUMS`.
2. Submit a false-positive report:
   - Microsoft Defender: <https://www.microsoft.com/en-us/wdsi/filesubmission>
   - Other vendors accept submissions through their research portals.
3. Prefer reputable distribution channels — **winget**, **scoop**, **choco** —
   whose package reputation carries over and shortens the SmartScreen warm-up.
