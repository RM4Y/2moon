// Windows: the 2moon icon and version information in the executable (Explorer,
// Task Manager, shortcuts). The resource script is generated with absolute paths.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest.join("../../installer/windows/2moon.ico").canonicalize().expect("installer/windows/2moon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let v = std::env::var("CARGO_PKG_VERSION").unwrap();
    let n: Vec<u32> = v.split(['.', '-']).take(3).map(|p| p.parse().unwrap_or(0)).collect();
    let (a, b, c) = (n[0], n.get(1).copied().unwrap_or(0), n.get(2).copied().unwrap_or(0));
    let rc = format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "2moon"
      VALUE "FileDescription", "2moon"
      VALUE "FileVersion", "{v}"
      VALUE "InternalName", "moon-client.exe"
      VALUE "LegalCopyright", "GPL-3.0-or-later"
      VALUE "OriginalFilename", "moon-client.exe"
      VALUE "ProductName", "2moon"
      VALUE "ProductVersion", "{v}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        // windres reads backslashes as escapes: on a Windows build machine, drop the
        // \\?\ prefix of canonicalize() and use forward slashes.
        icon = icon.display().to_string().trim_start_matches(r"\\?\").replace('\\', "/")
    );
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("app.rc");
    std::fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE).manifest_optional().unwrap();
}
