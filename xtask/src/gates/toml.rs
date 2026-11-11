//! The little of TOML the gates read from a `Cargo.toml`: section headers and
//! `key = value` entries, a value running on while its brackets are open.
//! Comments are dropped. It does not interpret values; a gate asks for the
//! strings in one ([`strings`]) or looks at its text.

#[derive(Debug, Default)]
pub(super) struct Section {
    /// The text between the brackets: `dependencies`, `target.'cfg(unix)'.dependencies`.
    pub(super) header: String,
    pub(super) entries: Vec<(String, String)>,
}

#[derive(Debug, Default)]
pub(super) struct Toml {
    pub(super) sections: Vec<Section>,
}

/// Which dependency table a header names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Normal,
    Build,
    Dev,
}

impl Kind {
    pub(super) fn label(self) -> &'static str {
        match self {
            Kind::Normal => "normal",
            Kind::Build => "build",
            Kind::Dev => "dev",
        }
    }
}

impl Toml {
    pub(super) fn parse(text: &str) -> Toml {
        let mut t = Toml::default();
        // The root keys before any header live in a section with an empty header.
        t.sections.push(Section::default());
        let mut lines = text.lines();
        while let Some(raw) = lines.next() {
            let line = strip_comment(raw);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                let header = line.trim_start_matches('[').trim_end_matches(']').trim();
                t.sections.push(Section {
                    header: header.to_owned(),
                    entries: Vec::new(),
                });
                continue;
            }
            let Some(eq) = line.find('=') else { continue };
            let key = line[..eq].trim().to_owned();
            let mut value = line[eq + 1..].trim().to_owned();
            while depth(&value) > 0 {
                let Some(next) = lines.next() else { break };
                value.push(' ');
                value.push_str(strip_comment(next).trim());
            }
            t.sections
                .last_mut()
                .expect("one section is always there")
                .entries
                .push((key, value));
        }
        t
    }

    pub(super) fn section(&self, header: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.header == header)
    }

    pub(super) fn value(&self, header: &str, key: &str) -> Option<&str> {
        self.section(header)?
            .entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Every dependency the manifest declares, in every table (`[dependencies]`,
    /// `[dev-dependencies]`, `[build-dependencies]`, their `target` forms and a
    /// `[dependencies.name]` table), as `(package name, kind)`. A `package =`
    /// rename names the package, as `cargo metadata` does. `[workspace.dependencies]`
    /// is not a dependency of the manifest.
    pub(super) fn dependencies(&self) -> Vec<(String, Kind)> {
        let mut out = Vec::new();
        for s in &self.sections {
            let Some((kind, table)) = dependency_header(&s.header) else {
                continue;
            };
            match table {
                Some(name) => {
                    let package = s
                        .entries
                        .iter()
                        .find(|(k, _)| k == "package")
                        .and_then(|(_, v)| strings(v).into_iter().next());
                    out.push((package.unwrap_or(name), kind));
                }
                None => {
                    for (k, v) in &s.entries {
                        let name = unquote(k.split('.').next().unwrap_or(k));
                        let package = if let Some(rest) = k.split_once('.') {
                            (rest.1 == "package")
                                .then(|| strings(v).into_iter().next())
                                .flatten()
                        } else {
                            inline_package(v)
                        };
                        out.push((package.unwrap_or(name), kind));
                    }
                }
            }
        }
        out
    }
}

/// `(kind, Some(name))` for `[dependencies.name]`, `(kind, None)` for
/// `[dependencies]`, through any `target.<cfg>.` prefix.
fn dependency_header(header: &str) -> Option<(Kind, Option<String>)> {
    let segs = split_dots(header);
    if segs.first().map(String::as_str) == Some("workspace") {
        return None;
    }
    let kind_of = |s: &str| match s {
        "dependencies" => Some(Kind::Normal),
        "build-dependencies" => Some(Kind::Build),
        "dev-dependencies" => Some(Kind::Dev),
        _ => None,
    };
    let target = segs.first().map(String::as_str) == Some("target");
    let at = if target { 2 } else { 0 };
    let kind = kind_of(segs.get(at)?)?;
    match segs.len() - at {
        1 => Some((kind, None)),
        2 => Some((kind, Some(segs[at + 1].clone()))),
        _ => None,
    }
}

/// `package = "x"` inside an inline table value.
fn inline_package(value: &str) -> Option<String> {
    let at = value.find("package")?;
    let rest = value[at + "package".len()..]
        .trim_start()
        .strip_prefix('=')?;
    strings(rest).into_iter().next()
}

/// A dotted key's segments, a quoted segment unquoted.
fn split_dots(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => quote = Some(c),
            (None, '.') => out.push(std::mem::take(&mut cur)),
            (None, c) => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_owned()).collect()
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches(['"', '\'']).to_owned()
}

/// The quoted strings of a value, in order.
pub(super) fn strings(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Option<(char, String)> = None;
    for c in value.chars() {
        match (&mut cur, c) {
            (Some((q, _)), c) if c == *q => out.push(cur.take().expect("open").1),
            (Some((_, s)), c) => s.push(c),
            (None, '"' | '\'') => cur = Some((c, String::new())),
            (None, _) => {}
        }
    }
    out
}

/// The line without its `#` comment (a `#` inside a string is text).
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '#') => return &line[..i],
            (None, _) => {}
        }
    }
    line
}

/// How many brackets a value leaves open.
fn depth(value: &str) -> i32 {
    let mut quote: Option<char> = None;
    let mut d = 0;
    for c in value.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '[' | '{') => d += 1,
            (None, ']' | '}') => d -= 1,
            (None, _) => {}
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &str = r##"
[package]
name = "p" # a comment, and a "#" in text: "a#b"
[features]
decode = [
    "dep:symphonia", # why
    "x/y",
]
[dependencies]
a = { path = "../a" }
b.workspace = true
c = { package = "real", version = "1" }
[dev-dependencies.d]
path = "../d"
[target.'cfg(unix)'.build-dependencies]
e = "1"
[workspace.dependencies]
f = "1"
"##;

    #[test]
    fn sections_entries_and_multiline_values() {
        let t = Toml::parse(M);
        assert_eq!(t.value("package", "name"), Some("\"p\""));
        assert_eq!(
            strings(t.value("features", "decode").unwrap()),
            ["dep:symphonia", "x/y"]
        );
    }

    #[test]
    fn dependencies_of_every_table_with_renames() {
        let mut d = Toml::parse(M).dependencies();
        d.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            d,
            [
                ("a".to_owned(), Kind::Normal),
                ("b".to_owned(), Kind::Normal),
                ("d".to_owned(), Kind::Dev),
                ("e".to_owned(), Kind::Build),
                ("real".to_owned(), Kind::Normal),
            ]
        );
    }
}
