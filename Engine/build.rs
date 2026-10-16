//! The installed binary must carry its own user help, never depend on a checkout.
use std::path::{Path, PathBuf};

fn collect(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) {
    println!("cargo:rerun-if-changed={}", dir.display());
    for entry in std::fs::read_dir(dir).expect("bundled help directory") {
        let entry = entry.expect("bundled help entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        // An editor's swap or backup file and an OS's folder note (.DS_Store,
        // .directory) appear while the guide is being edited; they are never
        // help and never stop a build.
        if name.starts_with('.') || name.ends_with('~') {
            continue;
        }
        let kind = entry.file_type().expect("bundled help file type");
        let path = entry.path();
        if kind.is_dir() {
            collect(root, &path, files);
        } else {
            assert!(
                kind.is_file() && path.extension().is_some_and(|e| e == "md"),
                "bundled help contains a link, script or non-Markdown file: {}",
                path.display()
            );
            files.push(path.strip_prefix(root).unwrap().to_owned());
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest
        .parent()
        .unwrap()
        .join(".rune/skills/rune-settings-guide");
    let mut files = Vec::new();
    collect(&root, &root, &mut files);
    files.sort();
    assert!(files.iter().any(|p| p == Path::new("SKILL.md")));
    let mut code = String::from("pub const DOCUMENTS: &[(&str, &str)] = &[\n");
    for relative in files {
        let name = relative.to_str().unwrap().replace('\\', "/");
        code.push_str(&format!(
            "({name:?}, include_str!({:?})),\n",
            root.join(relative)
        ));
    }
    code.push_str("];\n");
    std::fs::write(
        PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("product_help.rs"),
        code,
    )
    .expect("write bundled help manifest");
}
