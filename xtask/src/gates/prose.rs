//! The repo's prose, as the doc gates read it: the comments of every `.rs`
//! file and the whole text of every `.md` file.
//!
//! The prose is read from `crates/`, `testing/`, `xtask/`, `docs/` (not
//! `docs/design/` or `docs/archive/`, which are their own record) and the
//! root's [`ROOT_FILES`]. Each
//! text keeps its file's offsets, so a byte offset gives the file's line.

use super::Tree;
use super::lex::comments;

const ROOTS: [&str; 4] = ["crates", "testing", "xtask", "docs"];

/// The root's prose files; an absent one is skipped.
const ROOT_FILES: [&str; 4] = ["README.md", "CLAUDE.md", "CONTRIBUTING.md", "SECURITY.md"];

/// Directories never entered, wherever they sit.
fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git" | "node_modules")
}

/// Files under `docs/` that are a record rather than prose about the code.
fn record(path: &str) -> bool {
    path.starts_with("docs/design/") || path.starts_with("docs/archive/")
}

/// One file's prose.
pub(super) struct Prose {
    pub(super) path: String,
    /// The comments (a `.rs` file) or the text (a `.md` file), at the file's
    /// offsets.
    pub(super) text: String,
}

/// Every prose file the doc gates read, sorted by path.
pub(super) fn prose(tree: &dyn Tree) -> Vec<Prose> {
    let mut out = Vec::new();
    let mut paths: Vec<String> = ROOTS
        .iter()
        .flat_map(|r| tree.files(r, &skipped))
        .filter(|p| !record(p))
        .collect();
    paths.extend(ROOT_FILES.iter().map(|p| (*p).to_owned()));
    paths.sort();
    for path in paths {
        let Some(raw) = tree.read(&path) else {
            continue;
        };
        let text = if path.ends_with(".rs") {
            comments(&raw)
        } else if path.ends_with(".md") {
            raw
        } else {
            continue;
        };
        out.push(Prose { path, text });
    }
    out
}

/// A byte that continues an identifier.
pub(super) fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    #[test]
    fn comments_of_rust_and_text_of_markdown_outside_the_records() {
        let tree = Mem::with(&[
            ("crates/a/src/lib.rs", "let s = \"code\"; // prose\n"),
            ("crates/a/README.md", "readme\n"),
            ("crates/a/Cargo.toml", "# toml\n"),
            ("docs/design/013-x.md", "record\n"),
            ("docs/style-guide.md", "guide\n"),
            ("README.md", "root\n"),
            ("CHANGELOG.md", "history\n"),
        ]);
        let got: Vec<(String, String)> = prose(&tree)
            .into_iter()
            .map(|p| (p.path, p.text.trim().to_owned()))
            .collect();
        assert_eq!(
            got,
            [
                ("README.md".to_owned(), "root".to_owned()),
                ("crates/a/README.md".to_owned(), "readme".to_owned()),
                ("crates/a/src/lib.rs".to_owned(), "// prose".to_owned()),
                ("docs/style-guide.md".to_owned(), "guide".to_owned()),
            ]
        );
    }
}
