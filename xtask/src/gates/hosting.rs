//! `check-hosting`: every hosting marker a format implements is listed in the
//! plugin contract (design 021 section 3.3).
//!
//! `impl InProcess for X` or `impl OutOfProcess for X` in a crate's `src/`
//! needs a contract suite whose format's loaded type is `X` and whose
//! `contract!(.., hostings = [..])` names that marker. The other direction is
//! the compiler's: `contract!` asserts each marker it lists, so a listed
//! marker without its impl does not build.
//!
//! The markers are safe and unsealed, so an impl is a promise no type checks;
//! the contract's cases are what hold a format to it. An impl the contract
//! does not list would be hostable without being tested in that hosting.
//!
//! Limits: a textual scan. It sees `impl [<..>] [path::]InProcess for [path::]Name`
//! on one line in `crates/*/src`, maps `Name` to a contract format through
//! `type L = ..::Name;` in `testing/tutti-plugin-contract/src/formats.rs`, and
//! reads the `contract!` calls in `testing/tutti-plugin-contract/tests/*.rs`
//! (which may span lines).

use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

const MARKERS: [&str; 2] = ["InProcess", "OutOfProcess"];
const FORMATS: &str = "testing/tutti-plugin-contract/src/formats.rs";
const SUITES: &str = "testing/tutti-plugin-contract/tests";

fn skipped(dir: &str) -> bool {
    dir == "target"
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The `[\w:]*` run at the start of `s`.
fn path_run(s: &str) -> &str {
    let n = s
        .find(|c: char| !(is_word(c) || c == ':'))
        .unwrap_or(s.len());
    &s[..n]
}

/// The last `::` segment of a path.
fn last_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

/// `(marker, type)` of a line `impl [<..>] [path::]Marker for [path::]Type`.
fn impl_line(line: &str) -> Option<(&'static str, String)> {
    let mut rest = line.trim_start().strip_prefix("impl")?;
    let had_ws = rest.starts_with(char::is_whitespace);
    rest = rest.trim_start();
    if rest.starts_with('<') {
        rest = rest[rest.find('>')? + 1..].trim_start();
    } else if !had_ws {
        return None;
    }
    // After generics `\s+` is required, and `trim_start` took it with the `<..>`'s.
    let marker_path = path_run(rest);
    let marker = MARKERS
        .into_iter()
        .find(|m| last_segment(marker_path) == *m)?;
    if marker_path != marker && !marker_path.ends_with(&format!("::{marker}")) {
        return None;
    }
    let after = &rest[marker_path.len()..];
    let after_ws = after.trim_start();
    if after_ws.len() == after.len() {
        return None;
    }
    let after = after_ws.strip_prefix("for")?;
    let target = after.trim_start();
    if target.len() == after.len() {
        return None;
    }
    let name = last_segment(path_run(target));
    let name: String = name.chars().take_while(|&c| is_word(c)).collect();
    (!name.is_empty()).then_some((marker, name))
}

/// `type L = [path::]Name;` in a block.
fn loaded_type(block: &str) -> Option<String> {
    let mut at = 0;
    while let Some(p) = block[at..].find("type") {
        let s = at + p;
        at = s + 4;
        let before_ok = s == 0 || !block[..s].ends_with(is_word);
        let rest = &block[s + 4..];
        let r = rest.trim_start();
        if !before_ok || r.len() == rest.len() {
            continue;
        }
        let Some(r) = r.strip_prefix('L') else {
            continue;
        };
        let Some(r) = r.trim_start().strip_prefix('=') else {
            continue;
        };
        let target = r.trim_start();
        let name = last_segment(path_run(target));
        let name: String = name.chars().take_while(|&c| is_word(c)).collect();
        if !name.is_empty()
            && target[path_run(target).len()..]
                .trim_start()
                .starts_with(';')
        {
            return Some(name);
        }
    }
    None
}

/// `impl Format for Name { .. }` blocks (to the first `\n}`), as `(Name, body)`.
fn format_impls(src: &str) -> Vec<(String, &str)> {
    let mut out = Vec::new();
    for (at, _) in src.match_indices("impl") {
        let rest = &src[at + 4..];
        let r = rest.trim_start();
        if r.len() == rest.len() {
            continue;
        }
        let Some(r) = r.strip_prefix("Format") else {
            continue;
        };
        let r2 = r.trim_start();
        let Some(r2) = r2.strip_prefix("for") else {
            continue;
        };
        let r3 = r2.trim_start();
        if r3.len() == r2.len() {
            continue;
        }
        let name: String = r3.chars().take_while(|&c| is_word(c)).collect();
        let Some(body) = r3[name.len()..].trim_start().strip_prefix('{') else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let end = body.find("\n}");
        if let Some(end) = end {
            out.push((name, &body[..end]));
        }
    }
    out
}

/// `(format, markers)` of each `contract!(Format, .., hostings = [..]);`.
fn contracts(text: &str) -> Vec<(String, BTreeSet<String>)> {
    let mut out = Vec::new();
    for (at, _) in text.match_indices("contract!") {
        let rest = text[at + "contract!".len()..].trim_start();
        let Some(args) = rest.strip_prefix('(') else {
            continue;
        };
        let mut end = None;
        for (i, c) in args.char_indices() {
            if c == ')' && args[i + 1..].trim_start().starts_with(';') {
                end = Some(i);
                break;
            }
        }
        let Some(end) = end else { continue };
        let args = &args[..end];
        let Some(comma) = args.find(',') else {
            continue;
        };
        let fmt = last_segment(args[..comma].trim());
        if fmt.is_empty() || !fmt.chars().all(is_word) {
            continue;
        }
        let mut names = BTreeSet::new();
        if let Some(h) = args.find("hostings") {
            let r = args[h + "hostings".len()..].trim_start();
            if let Some(r) = r.strip_prefix('=')
                && let Some(r) = r.trim_start().strip_prefix('[')
                && let Some(close) = r.find(']')
            {
                for w in r[..close].split(|c: char| !is_word(c)) {
                    if !w.is_empty() {
                        names.insert(w.to_owned());
                    }
                }
            }
        }
        out.push((fmt.to_owned(), names));
    }
    out
}

pub(super) fn check(tree: &dyn Tree) -> Report {
    let mut impls: Vec<(&'static str, String, String)> = Vec::new();
    for path in tree.files("crates", &skipped) {
        let in_src = path
            .strip_prefix("crates/")
            .and_then(|r| r.split_once('/'))
            .is_some_and(|(_, r)| r.starts_with("src/"));
        if !in_src || !path.ends_with(".rs") {
            continue;
        }
        let Some(text) = tree.read(&path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            if let Some((marker, ty)) = impl_line(line) {
                impls.push((marker, ty, format!("{path}:{}", n + 1)));
            }
        }
    }

    let mut formats: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let src = tree.read(FORMATS).unwrap_or_default();
    for (name, body) in format_impls(&src) {
        if let Some(t) = loaded_type(body) {
            formats.entry(t).or_default().push(name);
        }
    }

    let mut listed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in tree.files(SUITES, &skipped) {
        if path[SUITES.len() + 1..].contains('/') || !path.ends_with(".rs") {
            continue;
        }
        let text = tree.read(&path).unwrap_or_default();
        for (fmt, names) in contracts(&text) {
            listed.entry(fmt).or_default().extend(names);
        }
    }

    let mut r = Report::default();
    if impls.is_empty() {
        r.fail("check-hosting: found no InProcess/OutOfProcess impl; the scan is broken");
        return r;
    }
    let mut bad = Vec::new();
    for (marker, ty, at) in &impls {
        let Some(fmts) = formats.get(ty) else {
            bad.push(format!(
                "{at}: {ty} implements {marker}, but no contract format loads it"
            ));
            continue;
        };
        let any = fmts
            .iter()
            .any(|f| listed.get(f).is_some_and(|s| s.contains(*marker)));
        if !any {
            bad.push(format!(
                "{at}: {ty} implements {marker}, but no contract lists it \
                 (contract!({}, .., hostings = [.., {marker}]))",
                fmts.join("|")
            ));
        }
    }
    if bad.is_empty() {
        r.say(format!(
            "check-hosting: {} hosting impls, each listed in the contract",
            impls.len()
        ));
    } else {
        r.fail("check-hosting: FAIL");
        for b in bad {
            r.fail(format!("  {b}"));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const HOST: &str = "crates/tutti-clap-host/src/loaded.rs";
    const SUITE: &str = "testing/tutti-plugin-contract/tests/clap.rs";

    fn tree(host: &str, formats: &str, suite: &str) -> Mem {
        Mem::with(&[
            (HOST, host),
            (FORMATS, formats),
            (SUITE, suite),
            // Test files are not scanned for impls.
            (
                "crates/tutti-clap-host/tests/t.rs",
                "impl InProcess for Stray {}\n",
            ),
        ])
    }

    const FORMATS_OK: &str =
        "impl Format for Clap {\n    type L = tutti_clap_host::ClapLoaded;\n}\n";
    const SUITE_OK: &str =
        "contract!(\n    Clap,\n    gaps = 2,\n    hostings = [InProcess, OutOfProcess]\n);\n";
    const HOST_OK: &str = "impl InProcess for ClapLoaded {}\nimpl<'a> tutti_plugin_types::OutOfProcess for crate::ClapLoaded {}\n";

    #[test]
    fn impls_each_listed_in_the_contract_pass() {
        let r = check(&tree(HOST_OK, FORMATS_OK, SUITE_OK));
        assert_eq!(
            r.out,
            ["check-hosting: 2 hosting impls, each listed in the contract"]
        );
    }

    #[test]
    fn an_impl_the_contract_does_not_list_fails() {
        let suite = "contract!(Clap, gaps = 2, hostings = [InProcess]);\n";
        let r = check(&tree(HOST_OK, FORMATS_OK, suite));
        assert_eq!(r.findings[0], "check-hosting: FAIL");
        assert_eq!(
            r.findings[1],
            format!(
                "  {HOST}:2: ClapLoaded implements OutOfProcess, but no contract lists it (contract!(Clap, .., hostings = [.., OutOfProcess]))"
            )
        );
    }

    #[test]
    fn a_type_no_contract_format_loads_fails() {
        let formats = "impl Format for Clap {\n    type L = tutti_clap_host::ClapLoadedX;\n}\n";
        let r = check(&tree(HOST_OK, formats, SUITE_OK));
        assert_eq!(r.findings.len(), 3);
        assert!(
            r.findings[1]
                .contains("ClapLoaded implements InProcess, but no contract format loads it")
        );
    }

    #[test]
    fn a_scan_that_finds_no_impl_fails() {
        let r = check(&tree("// impl InProcess for X {}\n", FORMATS_OK, SUITE_OK));
        assert_eq!(
            r.findings,
            ["check-hosting: found no InProcess/OutOfProcess impl; the scan is broken"]
        );
    }
}
