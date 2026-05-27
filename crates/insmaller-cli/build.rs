//! Embed Windows version metadata + an application manifest into the binary.
//!
//! Two reasons, both about not looking like malware to Windows:
//!   1. A blank version resource (no CompanyName/ProductName/FileDescription)
//!      is a heuristic AV/SmartScreen penalty. Populate it.
//!   2. `requestedExecutionLevel=asInvoker` opts the exe out of Windows'
//!      "installer detection" heuristic, which otherwise forces a UAC elevation
//!      prompt on any unsigned binary whose name/contents look installer-ish.
//!
//! Best-effort: if the resource compiler is unavailable, warn and continue so
//! non-Windows and minimal toolchains still build.

fn main() {
    #[cfg(windows)]
    embed_windows_resources();
}

#[cfg(windows)]
fn embed_windows_resources() {
    let mut res = winresource::WindowsResource::new();
    // FileVersion/ProductVersion are populated from CARGO_PKG_* automatically.
    res.set("CompanyName", "Walang Studio");
    res.set("ProductName", "insmaller");
    res.set("FileDescription", "insmaller installer harness");
    res.set("OriginalFilename", "insmaller.exe");
    res.set("LegalCopyright", "Copyright (c) Walang Studio");
    res.set_manifest(MANIFEST);
    if let Err(e) = res.compile() {
        println!("cargo:warning=winresource: version resource not embedded: {e}");
    }
}

#[cfg(windows)]
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#;
