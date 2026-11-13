//! `cargo xtask release <version>`: the edits a release pull request makes.
//!
//! Every crate shares the workspace version. A release sets
//! `[workspace.package] version`, moves each `version = "<old>"` pin on a
//! workspace path dependency to the new version, and writes the changelog
//! section for it from the first-parent commit subjects since the previous
//! `v*` tag. Merging that pull request to `main` is the release: the release
//! workflow tags the merge and publishes its notes.
//!
//! The functions here are pure over the text they are given; [`run`] reads and
//! writes the files and asks `git` for the subjects.

use std::path::Path;
use std::process::Command;

/// A `MAJOR.MINOR.PATCH` version, with no pre-release or build suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Version(String);

impl Version {
    pub(crate) fn parse(s: &str) -> Option<Version> {
        let parts: Vec<&str> = s.split('.').collect();
        let numeric = |p: &&str| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))
        };
        (parts.len() == 3 && parts.iter().all(numeric)).then(|| Version(s.to_owned()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn display(&self) -> &str {
        &self.0
    }
}

/// The `version` under `[workspace.package]` in the root manifest.
fn workspace_version(root_manifest: &str) -> Option<&str> {
    let mut inside = false;
    for line in root_manifest.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t == "[workspace.package]";
        } else if inside && let Some(v) = t.strip_prefix("version = \"") {
            return v.strip_suffix('"');
        }
    }
    None
}

/// The root manifest with `[workspace.package] version` set to `new`.
fn set_workspace_version(root_manifest: &str, new: &Version) -> Result<String, String> {
    let old = workspace_version(root_manifest)
        .ok_or("the root manifest has no `[workspace.package] version`")?;
    let mut inside = false;
    let mut out = Vec::new();
    for line in root_manifest.split('\n') {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t == "[workspace.package]";
        }
        if inside && t == format!("version = \"{old}\"") {
            out.push(line.replace(old, new.as_str()));
        } else {
            out.push(line.to_owned());
        }
    }
    Ok(out.join("\n"))
}

/// A member manifest with every `version = "<old>"` pin inside an inline
/// table that also holds a `path = ` key moved to `new`. A version pin on a
/// registry dependency has no `path` and is left alone.
fn bump_path_pins(manifest: &str, old: &str, new: &Version) -> String {
    let pin = format!("version = \"{old}\"");
    manifest
        .split('\n')
        .map(|line| {
            let code = line.split('#').next().unwrap_or("");
            if code.contains('{') && code.contains("path = ") && code.contains(&pin) {
                line.replacen(&pin, &format!("version = \"{}\"", new.as_str()), 1)
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Where a squash-merged subject lands in the changelog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Breaking,
    Feature,
    Fix,
    Other,
}

impl Kind {
    fn heading(self) -> &'static str {
        match self {
            Kind::Breaking => "Breaking changes",
            Kind::Feature => "Features",
            Kind::Fix => "Fixes",
            Kind::Other => "Other changes",
        }
    }
}

/// A Conventional Commit subject (`feat(session)!: text (#12)`) as its kind
/// and the line the changelog shows (`session: text (#12)`). A subject in no
/// conventional form is `Other`, shown as written.
fn entry(subject: &str) -> (Kind, String) {
    let Some((head, text)) = subject.split_once(": ") else {
        return (Kind::Other, subject.to_owned());
    };
    let breaking = head.ends_with('!');
    let head = head.trim_end_matches('!');
    let (ty, scope) = match head.split_once('(') {
        Some((ty, rest)) => (ty, rest.strip_suffix(')')),
        None => (head, None),
    };
    if ty.is_empty() || !ty.bytes().all(|b| b.is_ascii_lowercase()) {
        return (Kind::Other, subject.to_owned());
    }
    let kind = if breaking {
        Kind::Breaking
    } else {
        match ty {
            "feat" => Kind::Feature,
            "fix" => Kind::Fix,
            _ => Kind::Other,
        }
    };
    let line = match scope {
        Some(s) if !s.is_empty() => format!("{s}: {text}"),
        _ => text.to_owned(),
    };
    (kind, line)
}

/// The changelog section for `version`, from subjects oldest first.
fn section(version: &Version, date: &str, subjects: &[String], first: bool) -> String {
    let mut out = format!("## {} - {date}\n", version.as_str());
    if first {
        out.push_str("\nThe first release.\n");
        return out;
    }
    let mut entries: Vec<(Kind, String)> = subjects.iter().map(|s| entry(s)).collect();
    entries.sort_by_key(|(k, _)| *k);
    let mut current = None;
    for (kind, line) in entries {
        if current != Some(kind) {
            out.push_str(&format!("\n### {}\n\n", kind.heading()));
            current = Some(kind);
        }
        out.push_str(&format!("- {line}\n"));
    }
    if current.is_none() {
        out.push_str("\nNo changes.\n");
    }
    out
}

/// The changelog with `section` placed right under its `## Unreleased`
/// heading, which stays (empty) for the next release.
fn insert_section(changelog: &str, section: &str) -> Result<String, String> {
    let at = changelog
        .find("## Unreleased\n")
        .ok_or("CHANGELOG.md has no `## Unreleased` heading")?;
    let after = at + "## Unreleased\n".len();
    let rest = changelog[after..].trim_start_matches('\n');
    Ok(format!(
        "{}## Unreleased\n\n{section}\n{rest}",
        &changelog[..at]
    ))
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Applies the release edits in the checkout at `root`.
pub(crate) fn run(root: &Path, version: &Version) -> Result<(), String> {
    let read = |p: &Path| {
        std::fs::read_to_string(p).map_err(|e| format!("cannot read {}: {e}", p.display()))
    };
    let write = |p: &Path, s: &str| {
        std::fs::write(p, s).map_err(|e| format!("cannot write {}: {e}", p.display()))
    };

    let root_manifest_path = root.join("Cargo.toml");
    let root_manifest = read(&root_manifest_path)?;
    let old = workspace_version(&root_manifest)
        .ok_or("the root manifest has no `[workspace.package] version`")?
        .to_owned();
    if old == version.as_str() {
        return Err(format!("the workspace is already at {old}"));
    }
    write(
        &root_manifest_path,
        &set_workspace_version(&root_manifest, version)?,
    )?;
    for top in ["crates", "testing"] {
        let Ok(dirs) = std::fs::read_dir(root.join(top)) else {
            continue;
        };
        for dir in dirs.flatten() {
            let path = dir.path().join("Cargo.toml");
            if let Ok(text) = std::fs::read_to_string(&path) {
                let bumped = bump_path_pins(&text, &old, version);
                if bumped != text {
                    write(&path, &bumped)?;
                }
            }
        }
    }

    let previous = git(root, &["describe", "--tags", "--abbrev=0", "--match", "v*"]).ok();
    let range = previous
        .as_deref()
        .map(|t| format!("{}..HEAD", t.trim()))
        .unwrap_or_else(|| "HEAD".to_owned());
    let log = git(
        root,
        &["log", "--first-parent", "--reverse", "--format=%s", &range],
    )?;
    let subjects: Vec<String> = log.lines().map(str::to_owned).collect();
    let date = git(root, &["log", "-1", "--format=%cs", "HEAD"])?;
    let changelog_path = root.join("CHANGELOG.md");
    let changelog = read(&changelog_path)?;
    let section = section(version, date.trim(), &subjects, previous.is_none());
    write(&changelog_path, &insert_section(&changelog, &section)?)?;
    println!(
        "release {}: workspace version {old} -> {}, changelog section written; open a pull request titled `release: v{}`",
        version.as_str(),
        version.as_str(),
        version.as_str()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn a_version_is_three_plain_numbers() {
        assert!(Version::parse("0.1.0").is_some());
        assert!(Version::parse("10.20.3").is_some());
        for bad in [
            "0.1",
            "0.1.0.0",
            "v0.1.0",
            "0.01.0",
            "0.1.0-rc.1",
            "0..1",
            "",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_workspace_version_moves_and_nothing_else_does() {
        let m = "[package]\nversion = \"0.0.1\"\n[workspace.package]\nedition = \"2024\"\nversion = \"0.0.1\"\n[x]\nversion = \"0.0.1\"\n";
        assert_eq!(
            set_workspace_version(m, &v("0.1.0")).unwrap(),
            "[package]\nversion = \"0.0.1\"\n[workspace.package]\nedition = \"2024\"\nversion = \"0.1.0\"\n[x]\nversion = \"0.0.1\"\n"
        );
        assert!(set_workspace_version("[workspace]\n", &v("0.1.0")).is_err());
    }

    #[test]
    fn a_path_pin_moves_and_a_registry_pin_stays() {
        let m = "tutti-types = { version = \"0.0.1\", path = \"../tutti-types\" }\nfoo = { version = \"0.0.1\" }\ntutti-rt = { path = \"../tutti-rt\" }\n# tutti-x = { version = \"0.0.1\", path = \"x\" }\n";
        assert_eq!(
            bump_path_pins(m, "0.0.1", &v("0.1.0")),
            "tutti-types = { version = \"0.1.0\", path = \"../tutti-types\" }\nfoo = { version = \"0.0.1\" }\ntutti-rt = { path = \"../tutti-rt\" }\n# tutti-x = { version = \"0.0.1\", path = \"x\" }\n"
        );
    }

    #[test]
    fn subjects_group_by_kind_with_the_scope_kept() {
        let subjects: Vec<String> = [
            "docs: the README (#791)",
            "fix(graph): a tap reads its port (#12)",
            "feat(session)!: typed keys (#759)",
            "feat: a new node (#13)",
            "Merge something odd",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(
            section(&v("0.2.0"), "2026-12-01", &subjects, false),
            "## 0.2.0 - 2026-12-01\n\n### Breaking changes\n\n- session: typed keys (#759)\n\n### Features\n\n- a new node (#13)\n\n### Fixes\n\n- graph: a tap reads its port (#12)\n\n### Other changes\n\n- the README (#791)\n- Merge something odd\n"
        );
        assert_eq!(
            section(&v("0.1.0"), "2026-12-01", &subjects, true),
            "## 0.1.0 - 2026-12-01\n\nThe first release.\n"
        );
    }

    #[test]
    fn the_section_goes_under_unreleased_which_stays() {
        let c = "# Changelog\n\n## Unreleased\n\nOlder text.\n";
        assert_eq!(
            insert_section(c, "## 0.1.0 - d\n\nThe first release.\n").unwrap(),
            "# Changelog\n\n## Unreleased\n\n## 0.1.0 - d\n\nThe first release.\n\nOlder text.\n"
        );
        assert!(insert_section("# Changelog\n", "x").is_err());
    }
}
