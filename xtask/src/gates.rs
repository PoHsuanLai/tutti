//! The repo's static gates (they began as `scripts/check-*.sh`).
//!
//! A gate is a pure function from the repo's files to a [`Report`]. The files
//! come through [`Tree`] (and `git`'s answers through [`Git`]), so a test hands
//! a gate an in-memory tree and asserts the exact findings; the runner (in
//! `main.rs`) reads the real tree, prints [`Report::out`] and exits with the
//! status the script had. Std only, so the matching each script did with
//! `grep -E` or a Python regex is plain string code, written to the same
//! language the pattern accepted (each gate's docs say how).

mod canonical_paths;
mod clap_sys;
mod conformance;
mod conformance_site;
mod decode_boundary;
mod doc_features;
mod doc_refs;
mod errors;
mod ffi;
mod guards;
mod hosting;
mod junit;
mod layout;
mod lex;
mod lints;
mod midi_spec;
mod platform_seam;
mod pointers;
mod prose;
mod range_types;
mod session;
mod sf2_spec;
mod spec_text;
mod submodule_pins;
mod test_paths;
mod toml;
mod value_traits;
mod vst3_sdk;

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
    /// Every error type derives `thiserror::Error`.
    Errors,
    /// Every host callback runs under the panic guard.
    Guards,
    /// Every hosting impl is listed in the plugin contract.
    Hosting,
    /// `tutti-session` reaches no Bevy crate and re-exports no crate whole.
    Session,
    /// Every crate takes the workspace lints, and `missing_debug_implementations` denies.
    Lints,
    /// A fieldless public `Eq` enum is `Hash`.
    ValueTraits,
    /// Prose names crate paths and design-doc sections that exist.
    DocRefs,
    /// Prose outside `docs/` points at no `file.rs:N`; under `docs/` the line exists.
    Pointers,
    /// A platform cfg stands only on a `mod` declaration.
    PlatformSeam,
    /// The spec inventories are well formed, tags name rows, CLAP matches `clap-sys`.
    Conformance,
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
            Gate::Errors => "gate check-errors".to_owned(),
            Gate::Guards => "gate check-guards".to_owned(),
            Gate::Hosting => "gate check-hosting".to_owned(),
            Gate::Session => "gate check-session".to_owned(),
            Gate::Lints => "gate check-lints".to_owned(),
            Gate::ValueTraits => "gate check-value-traits".to_owned(),
            Gate::DocRefs => "gate check-doc-refs".to_owned(),
            Gate::Pointers => "gate check-pointers".to_owned(),
            Gate::PlatformSeam => "gate check-platform-seam".to_owned(),
            Gate::Conformance => "gate check-conformance".to_owned(),
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
            Gate::Errors => errors::check(&tree, errors::ALLOW),
            Gate::Guards => guards::check(&tree, guards::MIN_SEEN),
            Gate::Hosting => hosting::check(&tree),
            Gate::Session => session::check(&tree, &RealCargo(root.to_path_buf())),
            Gate::Lints => lints::check(&tree, lints::UNLINTED),
            Gate::ValueTraits => {
                value_traits::check(&tree, value_traits::ALLOW, value_traits::MIN_FILES)
            }
            Gate::DocRefs => doc_refs::check(&tree),
            Gate::Pointers => pointers::check(&tree),
            Gate::PlatformSeam => platform_seam::check(&tree, platform_seam::ALLOW),
            Gate::Conformance => {
                let sys = clap_sys_tree(&tree);
                let require =
                    conformance::Require::parse(std::env::var("TUTTI_CONFORMANCE").ok().as_deref());
                conformance::check(
                    &tree,
                    sys.as_ref().map(|t| t as &dyn Tree).map_err(Clone::clone),
                    &midi_spec_text,
                    &sf2_spec_text,
                    require,
                )
            }
        }
    }
}

/// The `clap-sys` crate's source in the cargo registry, at the version
/// `Cargo.lock` pins (`CARGO_HOME` defaults to `~/.cargo`); the error names
/// what is missing.
fn clap_sys_tree(repo: &RealTree) -> Result<RealTree, String> {
    let lock = repo.read("Cargo.lock").ok_or("Cargo.lock is missing")?;
    let ver = conformance::pinned_clap_sys(&lock).ok_or("Cargo.lock pins no clap-sys")?;
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .ok_or("neither CARGO_HOME nor HOME is set")?;
    let registry = home.join("registry/src");
    let found = fs::read_dir(&registry)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join(format!("clap-sys-{ver}")))
        .find(|p| p.is_dir());
    found.map(|root| RealTree { root }).ok_or_else(|| {
        format!(
            "clap-sys {ver} source not found at {}/*/clap-sys-{ver} (cargo fetch --locked)",
            registry.display()
        )
    })
}

/// The text of the spec `doc` names: the `.txt` of it in the
/// `TUTTI_MIDI_SPECS` directory (searched recursively), or else what
/// `pdftotext -layout` makes of the PDF there. The error names what is
/// missing.
fn midi_spec_text(doc: &midi_spec::Doc) -> Result<String, String> {
    spec_text("TUTTI_MIDI_SPECS", doc.source)
}

/// The text of the SoundFont 2.04 spec, from the `TUTTI_SF2_SPEC` directory
/// the same way.
fn sf2_spec_text() -> Result<String, String> {
    spec_text(sf2_spec::ENV, sf2_spec::SOURCE)
}

/// The `.txt` whose name starts with `source` under the directory the
/// variable `var` names, or else what `pdftotext -layout` makes of the `.pdf`.
fn spec_text(var: &str, source: &str) -> Result<String, String> {
    let dir = std::env::var_os(var)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{var} is not set"))?;
    let all = RealTree { root: dir.clone() }.files(".", &|_| false);
    let find = |ext: &str| {
        all.iter()
            .find(|p| {
                let name = p.rsplit('/').next().unwrap_or(p);
                name.starts_with(source) && name.ends_with(ext)
            })
            .cloned()
    };
    if let Some(txt) = find(".txt") {
        let bytes = fs::read(dir.join(&txt)).map_err(|e| format!("cannot read {txt}: {e}"))?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    let pdf = find(".pdf")
        .ok_or_else(|| format!("no {source}* .txt or .pdf under {} ({var})", dir.display()))?;
    let out = process::Command::new("pdftotext")
        .args(["-layout"])
        .arg(dir.join(&pdf))
        .arg("-")
        .output()
        .map_err(|e| format!("cannot run pdftotext on {pdf}: {e}"))?;
    if !out.status.success() {
        return Err(format!("pdftotext failed on {pdf}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `conformance --results <dir> --out <dir>`: the dashboard at `<out>/index.html`,
/// the landing page beside it at `<out>/../index.html` and the badges at `<out>/../badges/<area>-<os>.json`, from the JUnit files
/// `<dir>/<os>.xml` (an OS with no file has run nothing).
pub(crate) fn conformance_site(root: &Path, results: &Path, out: &Path) -> Result<(), String> {
    let mut texts = Vec::new();
    let entries = fs::read_dir(results).map_err(|e| {
        format!(
            "cannot read the results directory {}: {e}",
            results.display()
        )
    })?;
    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    files.sort();
    for path in files {
        if path.extension().is_some_and(|x| x == "xml")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            let os = conformance_site::Os::from_name(stem)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let xml =
                fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            texts.push((os, String::from_utf8_lossy(&xml).into_owned()));
        }
    }
    if texts.is_empty() {
        return Err(format!("no <os>.xml JUnit file in {}", results.display()));
    }
    let tree = RealTree {
        root: root.to_path_buf(),
    };
    let report = conformance_site::report(&tree, &texts)?;
    let site = out.parent().unwrap_or(out);
    let badges_dir = site.join("badges");
    let mut files = vec![
        (out.join("index.html"), conformance_site::dashboard(&report)),
        (site.join("index.html"), conformance_site::landing()),
    ];
    for (name, json) in conformance_site::badges(&report) {
        files.push((badges_dir.join(name), json));
    }
    for (path, contents) in files {
        fs::create_dir_all(path.parent().expect("a file path has a parent"))
            .and_then(|()| fs::write(&path, contents))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    for f in &report {
        for os in conformance_site::Os::ALL {
            let name = os.name();
            if !f.has_results(os) {
                println!("{} {name}: no results", f.name);
                continue;
            }
            let (covered, failing, applicable) = f.counts(os);
            println!(
                "{} {name}: {covered}/{applicable} covered, {failing} failing",
                f.name
            );
        }
    }
    Ok(())
}

/// `conformance-inventory <format>`: rewrite the format's inventory file from
/// its spec source, keeping the hand-written columns.
pub(crate) fn conformance_inventory(root: &Path, format: &str) -> Result<(), String> {
    match format {
        "clap" => clap_inventory(root),
        "midi" => midi_inventory(root),
        "sf2" => sf2_inventory(root),
        "vst3" => vst3_inventory(root),
        _ => Err(format!(
            "no inventory extractor for `{format}` (`clap`, `vst3`, `midi` or `sf2`)"
        )),
    }
}

fn vst3_inventory(root: &Path) -> Result<(), String> {
    let repo = RealTree {
        root: root.to_path_buf(),
    };
    let items = vst3_sdk::extract(&repo)?;
    let path = "testing/conformance/vst3.txt";
    let (text, added, removed) = conformance::regenerate_vst3(repo.read(path).as_deref(), &items)?;
    let file = root.join(path);
    fs::create_dir_all(file.parent().expect("a file path has a parent"))
        .and_then(|()| fs::write(&file, text))
        .map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    println!("wrote {path}: {} rows", items.len());
    for id in &added {
        println!("added {id} (no paraphrase: the gate refuses it until one is written)");
    }
    for id in &removed {
        println!("removed {id}: its row is gone from the file; check its tests' tags");
    }
    Ok(())
}

fn sf2_inventory(root: &Path) -> Result<(), String> {
    let repo = RealTree {
        root: root.to_path_buf(),
    };
    let spec = sf2_spec::extract(&sf2_spec_text()?)?;
    let path = "testing/conformance/sf2.txt";
    let (text, added, removed) =
        conformance::regenerate_sf2(&repo.read(path).unwrap_or_default(), &spec.items)?;
    let file = root.join(path);
    fs::create_dir_all(file.parent().expect("a file path has a parent"))
        .and_then(|()| fs::write(&file, text))
        .map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    println!("wrote {path}: {} extracted rows", spec.items.len());
    let text_of: std::collections::BTreeMap<&str, &str> = spec
        .items
        .iter()
        .map(|i| (i.id.as_str(), i.text.as_str()))
        .collect();
    for id in &added {
        println!(
            "added {id} (side todo, no paraphrase): {}",
            text_of[id.as_str()]
        );
    }
    for id in &removed {
        println!("removed {id}: its row is gone from the file; check its tests' tags");
    }
    Ok(())
}

fn midi_inventory(root: &Path) -> Result<(), String> {
    let repo = RealTree {
        root: root.to_path_buf(),
    };
    for doc in midi_spec::DOCS {
        midi_doc_inventory(root, &repo, doc)?;
    }
    Ok(())
}

fn midi_doc_inventory(root: &Path, repo: &RealTree, doc: &midi_spec::Doc) -> Result<(), String> {
    let items = midi_spec::extract(doc, &midi_spec_text(doc)?)?;
    let path = format!("testing/conformance/{}", doc.file);
    let (text, added, removed) =
        conformance::regenerate_midi(doc, repo.read(&path).as_deref(), &items)?;
    let file = root.join(&path);
    fs::create_dir_all(file.parent().expect("a file path has a parent"))
        .and_then(|()| fs::write(&file, text))
        .map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    println!("wrote {path}: {} rows", items.len());
    let text_of: std::collections::BTreeMap<&str, &str> = items
        .iter()
        .map(|i| (i.id.as_str(), i.text.as_str()))
        .collect();
    for id in &added {
        println!(
            "added {id} (side todo, no paraphrase): {}",
            text_of[id.as_str()]
        );
    }
    for id in &removed {
        println!("removed {id}: its row is gone from the file; check its tests' tags");
    }
    if !added.is_empty() || !removed.is_empty() {
        println!(
            "{} added, {} removed: a sentence inserted or dropped shifts the numbers after it, so check the paraphrases of the neighbours",
            added.len(),
            removed.len()
        );
    }
    Ok(())
}

fn clap_inventory(root: &Path) -> Result<(), String> {
    let repo = RealTree {
        root: root.to_path_buf(),
    };
    let sys = clap_sys_tree(&repo)?;
    let items = clap_sys::extract(&sys)?;
    let path = "testing/conformance/clap.txt";
    let text = conformance::regenerate(repo.read(path).as_deref(), &items)?;
    let file = root.join(path);
    fs::create_dir_all(file.parent().expect("a file path has a parent"))
        .and_then(|()| fs::write(&file, text))
        .map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    println!("wrote {path}: {} rows", items.len());
    Ok(())
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

    /// `cargo tree -i ogg_next_sys` for `export` alone and for `export,encode-vorbis`.
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
