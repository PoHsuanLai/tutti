//! One command to run: the program, its arguments and its environment.

use std::fmt::Write as _;

/// A process to run from the repo root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cmd {
    /// `NAME=value` assignments for this process only.
    pub(crate) env: Vec<(String, String)>,
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    /// An argument with a `*` is a glob the shell would have expanded
    /// (`profile-bench`); expand it when the command runs.
    pub(crate) glob: bool,
}

impl Cmd {
    /// Parses one recipe line: leading `NAME=value` words are the environment,
    /// then the program and its arguments. Single and double quotes group a
    /// word; there are no escapes, variables or operators.
    pub(crate) fn sh(line: &str) -> Cmd {
        let mut words = words(line)
            .unwrap_or_else(|e| panic!("{e}: {line}"))
            .into_iter();
        let mut env = Vec::new();
        let mut program = None;
        for w in words.by_ref() {
            match w.split_once('=') {
                Some((k, v)) if is_name(k) => env.push((k.to_owned(), v.to_owned())),
                _ => {
                    program = Some(w);
                    break;
                }
            }
        }
        Cmd {
            env,
            program: program.unwrap_or_else(|| panic!("no program: {line}")),
            args: words.collect(),
            glob: false,
        }
    }

    pub(crate) fn arg(mut self, a: impl Into<String>) -> Cmd {
        self.args.push(a.into());
        self
    }

    pub(crate) fn args<I, S>(mut self, a: I) -> Cmd
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(a.into_iter().map(Into::into));
        self
    }

    pub(crate) fn env(mut self, k: &str, v: impl Into<String>) -> Cmd {
        self.env.push((k.to_owned(), v.into()));
        self
    }

    /// The command as a shell would read it back.
    pub(crate) fn display(&self) -> String {
        let mut s = String::new();
        for (k, v) in &self.env {
            let _ = write!(s, "{k}={} ", quote(v));
        }
        s.push_str(&quote(&self.program));
        for a in &self.args {
            s.push(' ');
            s.push_str(&quote(a));
        }
        s
    }
}

fn is_name(k: &str) -> bool {
    !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Splits a line into words, honouring single and double quotes.
pub(crate) fn words(line: &str) -> Result<Vec<String>, &'static str> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote = None;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err("unterminated quote");
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

/// Single-quotes a word unless it is plainly safe (or a glob, which stays live).
pub(crate) fn quote(w: &str) -> String {
    let plain = !w.is_empty()
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:=,+@%-*".contains(c));
    if plain {
        w.to_owned()
    } else {
        format!("'{}'", w.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sh_splits_env_program_and_quoted_args() {
        let c = Cmd::sh(r#"A=1 B="x y" cargo nextest run -E 'not binary(a) and not binary(b)'"#);
        assert_eq!(
            c.env,
            [("A".into(), "1".into()), ("B".into(), "x y".into())]
        );
        assert_eq!(c.program, "cargo");
        assert_eq!(
            c.args,
            ["nextest", "run", "-E", "not binary(a) and not binary(b)"]
        );
    }

    #[test]
    fn an_equals_sign_after_the_program_is_an_argument() {
        let c = Cmd::sh("cargo run -- --x=1");
        assert!(c.env.is_empty());
        assert_eq!(c.args, ["run", "--", "--x=1"]);
    }

    #[test]
    fn display_round_trips_through_sh() {
        let c = Cmd::sh(r#"RUSTFLAGS="--cfg loom" cargo test -E 'a | b'"#);
        assert_eq!(Cmd::sh(&c.display()), c);
        assert_eq!(c.display(), "RUSTFLAGS='--cfg loom' cargo test -E 'a | b'");
    }

    #[test]
    fn an_unterminated_quote_is_refused() {
        assert!(words("cargo 'x").is_err());
    }
}
