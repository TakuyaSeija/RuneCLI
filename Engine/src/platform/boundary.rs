//! The boundary `platform/` draws, held by the gate: code anywhere else in
//! the workspace that speaks to the operating system directly — the engine,
//! the CLI, their unit and integration tests — fails here, naming the line.
//! No exceptions list: what a test needs of the OS is `platform::fixture`.

use std::path::{Path, PathBuf};

/// Why `line` speaks to the operating system directly, if it does. Comments
/// are prose; strings that only name a path (`"/proc"` in a deny-list, a
/// shell command a test classifies) are data, not a reach into the OS.
fn os_specific(line: &str) -> Option<&'static str> {
    let code = line.trim_start();
    if code.starts_with("//") {
        return None;
    }
    for (needle, what) in [
        ("libc::", "libc"),
        ("windows_sys::", "windows-sys"),
        ("std::os::", "std::os"),
        ("extern \"C\"", "a foreign function"),
        ("\"/proc/self", "procfs"),
        ("format!(\"/proc/", "procfs"),
        ("\"/proc/stat\"", "procfs"),
        ("/proc/$$", "procfs"),
        // Windows spells a canonical path with `\\?\` unless the platform
        // simplifies it: one spelling, or comparisons go wrong there.
        (".canonicalize()", "canonicalize (use platform::canonical)"),
        (
            "fs::canonicalize(",
            "canonicalize (use platform::canonical)",
        ),
    ] {
        if code.contains(needle) {
            return Some(what);
        }
    }
    let attribute = code.contains("cfg(") || code.contains("cfg!(") || code.contains("cfg_attr(");
    let os = [
        "unix",
        "windows",
        "target_os",
        "target_family",
        "target_vendor",
        "target_env",
    ];
    (attribute && os.iter().any(|name| code.contains(name)))
        .then_some("a cfg on the operating system")
}

/// Every Rust file of `dir`, `skip` and build output left out.
fn sources(dir: &Path, skip: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == skip || path.file_name().is_some_and(|name| name == "target") {
            continue;
        }
        if path.is_dir() {
            sources(&path, skip, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn os_specific_code_lives_only_in_platform() {
    let engine = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = engine.parent().expect("the engine sits in the workspace");
    let platform = engine.join("src/platform");
    let mut files = Vec::new();
    for crate_dir in [engine.to_path_buf(), workspace.join("Cli")] {
        sources(&crate_dir, &platform, &mut files);
    }
    assert!(
        files.len() > 100,
        "the scan found {} files: it is not looking at the workspace",
        files.len()
    );
    let mut leaks = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("a source file reads");
        for (number, line) in text.lines().enumerate() {
            if let Some(what) = os_specific(line) {
                leaks.push(format!(
                    "{}:{}: {what}: {}",
                    file.strip_prefix(workspace).unwrap_or(file).display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "operating-system code outside Engine/src/platform/ — move it behind the \
         platform contract (or, in a test, use platform::fixture):\n{}",
        leaks.join("\n")
    );
}

/// The scanner sees every way code reaches the OS, and leaves prose and
/// data alone — or the test above would pass by seeing nothing.
#[test]
fn the_scanner_tells_os_code_from_prose_and_data() {
    for code in [
        "let fd = unsafe { libc::dup(0) };",
        "use windows_sys::Win32::Foundation::HANDLE;",
        "let root = dir.canonicalize()?;",
        "let p = std::fs::canonicalize(&path).unwrap();",
        "use std::os::unix::fs::PermissionsExt;",
        "    use std::os::fd::AsRawFd;",
        "extern \"C\" fn on_signal(_: i32) {}",
        "let me = std::fs::metadata(\"/proc/self/exe\");",
        "let stat = std::fs::read_to_string(format!(\"/proc/{pid}/stat\"));",
        "let boot = std::fs::read_to_string(\"/proc/stat\");",
        ".args([\"-c\", \"ls /proc/$$/fd\"])",
        "#[cfg(unix)]",
        "#[cfg(not(target_os = \"linux\"))]",
        "if cfg!(windows) {",
        "#[cfg_attr(target_family = \"unix\", allow(dead_code))]",
    ] {
        assert!(os_specific(code).is_some(), "missed: {code}");
    }
    for prose_or_data in [
        "// Hand-written over `libc::` calls, see platform.",
        "    /// `std::os::unix` is never named outside platform.",
        "const PROTECTED: &[&str] = &[\"/etc\", \"/proc\", \"/sys\"];",
        "in_command(\"awk '/MemTotal/' /proc/meminfo\"),",
        "#[cfg(test)]",
        "#[cfg(any(test, feature = \"test-support\"))]",
        "let unix_like = true;",
    ] {
        assert!(
            os_specific(prose_or_data).is_none(),
            "flagged: {prose_or_data}"
        );
    }
}
