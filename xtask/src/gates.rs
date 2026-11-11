//! The repo's static gates (they began as `scripts/check-*.sh`; the one script left
//! is the parked adapter's, `scripts/parked/check-bevy-tutti.sh`).
//!
//! A gate is a pure function from the repo's files to a [`Report`]. The files
//! come through [`Tree`] (and `git`'s answers through [`Git`]), so a test hands
//! a gate an in-memory tree and asserts the exact findings; the runner (in
//! `main.rs`) reads the real tree, prints [`Report::out`] and exits with the
//! status the script had. Std only, so the matching each script did with
//! `grep -E` or a Python regex is plain string code, written to the same
//! language the pattern accepted (each gate's docs say how).

mod canonical_paths;
mod decode_boundary;
mod doc_features;
mod ffi;
mod guards;
mod hosting;
mod layout;
mod lex;
mod range_types;
mod session;
mod submodule_pins;
mod test_paths;
mod toml;

pub(crate) use doc_features::doc_runs;

use std::fs;
use std::path::{Path, PathBuf};
use std::process;

/// What a gate reads of the repo. Paths are repo-relative with `/`.
pub(crate) trait Tree {
    /// Every file under `dir`, recursively and sorted, not following links;
    /// a directory whose name `skip_dir` accepts is not entered. Empty when
    /// `dir` does not exist.
    fn files(&self, dir: &str, skip_dir: &dyn Fn(&str) -> bool) -> Vec<String>;
    /// The file's text (invalid UTF-8 is replaced), or `None` when absent.
    fn read(&self, path: &str) -> Option<String>;
}

/// What a gate asks of `git`.
pub(crate) trait Git {
    /// The stdout of `git submodule status -- <path>` with trailing newlines
    /// removed, or `None` when it is empty (or git failed).
    fn submodule_status(&self, path: &str) -> Option<String>;
}

/// What a gate asks of `cargo`.
pub(crate) trait Cargo {
    /// The stdout of `cargo tree --locked <args>`, or its stderr when cargo
    /// fails (a package that is not in the tree is a failure).
    fn tree(&self, args: &[&str]) -> Result<String, String>;
}

/// A gate's result: everything it prints, and the part that is a violation.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Report {
    /// The script's stdout, line by line.
    pub(crate) out: Vec<String>,
    /// The lines of `out` that report a violation; empty means the gate passed.
    pub(crate) findings: Vec<String>,
}

impl Report {
    fn say(&mut self, line: impl Into<String>) {
        self.out.push(line.into());
    }

    fn fail(&mut self, line: impl Into<String>) {
        let line = line.into();
        self.findings.push(line.clone());
        self.out.push(line);
    }

    pub(crate) fn passed(&self) -> bool {
        self.findings.is_empty()
    }
}

/// The gates that read only files (and git), as a recipe names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Gate {
    /// `check-paths [<crate> <module>...]`.
    CanonicalPaths(Vec<String>),
    RangeTypes,
    SubmodulePins,
    /// The repo layout, the layering and the codec boundary.
    Layout,
    /// `unsafe` lives only in the crates that hold it.
    Ffi,
    /// Every host callback runs under the panic guard.
    Guards,
    /// Every hosting impl is listed in the plugin contract.
    Hosting,
    /// `tutti-session` is Bevy-free and re-exports no crate whole.
    Session,
}

impl Gate {
    pub(crate) fn display(&self) -> String {
        match self {
            Gate::CanonicalPaths(a) if a.is_empty() => "gate check-canonical-paths".to_owned(),
            Gate::CanonicalPaths(a) => format!("gate check-canonical-paths {}", a.join(" ")),
            Gate::RangeTypes => "gate check-range-types".to_owned(),
            Gate::SubmodulePins => "gate check-submodule-pins".to_owned(),
            Gate::Layout => "gate check-layout".to_owned(),
            Gate::Ffi => "gate check-ffi".to_owned(),
            Gate::Guards => "gate check-guards".to_owned(),
            Gate::Hosting => "gate check-hosting".to_owned(),
            Gate::Session => "gate check-session".to_owned(),
        }
    }

    pub(crate) fn run(&self, root: &Path) -> Report {
        let tree = RealTree {
            root: root.to_path_buf(),
        };
        match self {
            Gate::CanonicalPaths(args) => canonical_paths::check(&tree, args),
            Gate::RangeTypes => range_types::check(&tree, range_types::EXCEPTIONS),
            Gate::SubmodulePins => {
                submodule_pins::check(&RealGit(root.to_path_buf()), submodule_pins::PINS)
            }
            Gate::Layout => layout::check(&tree, &RealCargo(root.to_path_buf())),
            Gate::Ffi => ffi::check(&tree),
            Gate::Guards => guards::check(&tree, guards::MIN_SEEN),
            Gate::Hosting => hosting::check(&tree),
            Gate::Session => session::check(&tree, &RealCargo(root.to_path_buf())),
        }
    }
}

/// The checkout on disk.
pub(crate) struct RealTree {
    pub(crate) root: PathBuf,
}

impl Tree for RealTree {
    fn files(&self, dir: &str, skip_dir: &dyn Fn(&str) -> bool) -> Vec<String> {
        fn walk(root: &Path, rel: &str, skip: &dyn Fn(&str) -> bool, out: &mut Vec<String>) {
            let Ok(rd) = fs::read_dir(root.join(rel)) else {
                return;
            };
            for e in rd.flatten() {
                let (Ok(ty), Ok(name)) = (e.file_type(), e.file_name().into_string()) else {
                    continue;
                };
                let path = format!("{rel}/{name}");
                if ty.is_dir() {
                    if !skip(&name) {
                        walk(root, &path, skip, out);
                    }
                } else if ty.is_file() {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, dir, skip_dir, &mut out);
        out.sort();
        out
    }

    fn read(&self, path: &str) -> Option<String> {
        let bytes = fs::read(self.root.join(path)).ok()?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// `git` run in the repo root.
pub(crate) struct RealGit(PathBuf);

impl Git for RealGit {
    fn submodule_status(&self, path: &str) -> Option<String> {
        let out = process::Command::new("git")
            .args(["submodule", "status", "--", path])
            .current_dir(&self.0)
            .stderr(process::Stdio::null())
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        let s = s.trim_end_matches('\n');
        (!s.is_empty()).then(|| s.to_owned())
    }
}

/// `cargo` run in the repo root.
pub(crate) struct RealCargo(PathBuf);

impl Cargo for RealCargo {
    fn tree(&self, args: &[&str]) -> Result<String, String> {
        let out = process::Command::new("cargo")
            .args(["tree", "--locked"])
            .args(args)
            .current_dir(&self.0)
            .output()
            .map_err(|e| format!("cannot run cargo: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }
}

/// Whether `path` has a directory component (not the file name) that `skip`
/// accepts: what `grep --exclude-dir` and `os.walk` pruning look at.
fn under_skipped_dir(path: &str, skip: &dyn Fn(&str) -> bool) -> bool {
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    parts.into_iter().any(skip)
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::BTreeMap;

    /// A tree of in-memory files, and a canned `git submodule status`.
    #[derive(Default)]
    pub(crate) struct Mem {
        pub(crate) files: BTreeMap<String, String>,
        pub(crate) status: BTreeMap<String, String>,
    }

    impl Mem {
        pub(crate) fn with(files: &[(&str, &str)]) -> Mem {
            Mem {
                files: files
                    .iter()
                    .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
                    .collect(),
                status: BTreeMap::new(),
            }
        }
    }

    impl Tree for Mem {
        fn files(&self, dir: &str, skip_dir: &dyn Fn(&str) -> bool) -> Vec<String> {
            let prefix = format!("{dir}/");
            self.files
                .keys()
                .filter(|p| p.starts_with(&prefix))
                .filter(|p| !under_skipped_dir(&p[prefix.len()..], skip_dir))
                .cloned()
                .collect()
        }

        fn read(&self, path: &str) -> Option<String> {
            self.files.get(path).cloned()
        }
    }

    /// A `cargo tree` that always answers `Err`.
    pub(crate) struct NoCargo;

    impl Cargo for NoCargo {
        fn tree(&self, _: &[&str]) -> Result<String, String> {
            Err("no cargo here".to_owned())
        }
    }

    /// A `cargo tree` that always prints this text.
    pub(crate) struct TreeText(pub(crate) &'static str);

    impl Cargo for TreeText {
        fn tree(&self, _: &[&str]) -> Result<String, String> {
            Ok(self.0.to_owned())
        }
    }

    /// `cargo tree -i ogg_next_sys` for `export` alone and for `export,export-ogg`.
    pub(crate) struct OggCargo {
        pub(crate) export_reaches: bool,
        pub(crate) ogg_reaches: bool,
    }

    impl Cargo for OggCargo {
        fn tree(&self, args: &[&str]) -> Result<String, String> {
            let features = args
                .iter()
                .position(|a| *a == "--features")
                .and_then(|i| args.get(i + 1))
                .copied();
            let reaches = if features == Some("export") {
                self.export_reaches
            } else {
                self.ogg_reaches
            };
            if reaches {
                Ok("ogg_next_sys v0.1
"
                .to_owned())
            } else {
                Err("error: package ID specification `ogg_next_sys` did not match".to_owned())
            }
        }
    }

    impl Git for Mem {
        fn submodule_status(&self, path: &str) -> Option<String> {
            self.status.get(path).cloned()
        }
    }
}
