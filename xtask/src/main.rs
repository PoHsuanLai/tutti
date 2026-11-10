//! `cargo xtask <recipe> [args]`: the repo's development commands, one
//! subcommand per recipe the justfile had, with the same names, commands,
//! environment and order. `cargo xtask help` lists them; `--dry-run` (before
//! the recipe) prints what would run.
//!
//! Std only, and no workspace dependency: the tool that builds and gates the
//! workspace does not depend on what it builds (`scripts/check-layout.sh`).

#![forbid(unsafe_code)]

mod cmd;
mod gates;
mod recipes;

use cmd::Cmd;
use recipes::{Ctx, RECIPES, Step};
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Help,
    Run {
        dry_run: bool,
        recipe: String,
        args: Vec<String>,
    },
}

/// `--dry-run` is read only before the recipe: after it, every word is the
/// recipe's own (`bench -- --test`).
fn parse(argv: &[String]) -> Result<Action, String> {
    let mut dry_run = false;
    let mut it = argv.iter().peekable();
    while let Some(a) = it.peek() {
        match a.as_str() {
            "--dry-run" | "-n" => {
                dry_run = true;
                it.next();
            }
            _ => break,
        }
    }
    match it.next().map(String::as_str) {
        None | Some("help" | "-h" | "--help") => Ok(Action::Help),
        Some(r) if r.starts_with('-') => Err(format!("unknown option `{r}`")),
        Some(r) => Ok(Action::Run {
            dry_run,
            recipe: r.to_owned(),
            args: it.cloned().collect(),
        }),
    }
}

fn help() -> String {
    let mut s = String::from(
        "usage: cargo xtask [--dry-run] <recipe> [args]\n\n  --dry-run   print the commands a recipe would run, run none\n\nrecipes:\n",
    );
    let heads: Vec<String> = RECIPES
        .iter()
        .map(|(name, args, _)| {
            if args.is_empty() {
                (*name).to_owned()
            } else {
                format!("{name} {args}")
            }
        })
        .collect();
    let width = heads.iter().map(String::len).max().unwrap_or(0);
    for (head, (_, _, doc)) in heads.iter().zip(RECIPES) {
        s.push_str(&format!("  {head:<width$}  {doc}\n"));
    }
    s
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (dry_run, recipe, args) = match parse(&argv) {
        Ok(Action::Help) => {
            print!("{}", help());
            return ExitCode::SUCCESS;
        }
        Ok(Action::Run {
            dry_run,
            recipe,
            args,
        }) => (dry_run, recipe, args),
        Err(e) => {
            eprintln!("xtask: {e}\n\n{}", help());
            return ExitCode::from(2);
        }
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits directly under the repo root")
        .to_path_buf();
    let target = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(t) => root.join(t), // an absolute path replaces the root
        None => root.join("target"),
    };
    let ctx = Ctx { root, target };
    let steps = match recipes::plan(&recipe, &args, &ctx) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("xtask: {e}");
            return ExitCode::from(2);
        }
    };
    for step in &steps {
        if dry_run {
            println!("{}", step.display());
            continue;
        }
        eprintln!("+ {}", step.display());
        if let Err(code) = execute(step, &ctx) {
            return ExitCode::from(code);
        }
    }
    ExitCode::SUCCESS
}

/// Runs one step; an error is the exit code to stop with.
fn execute(step: &Step, ctx: &Ctx) -> Result<(), u8> {
    match step {
        Step::Run(c) => run(c, ctx),
        Step::WriteFile { path, contents } => {
            let dir = path.parent().expect("a file path has a parent");
            std::fs::create_dir_all(dir)
                .and_then(|()| std::fs::write(path, contents))
                .map_err(|e| {
                    eprintln!("xtask: cannot write {}: {e}", path.display());
                    1
                })
        }
        Step::NoBevyInTree(c) => {
            let out = command(c, ctx)
                .stderr(process::Stdio::inherit())
                .output()
                .map_err(|e| spawn_failed(c, &e))?;
            let tree = String::from_utf8_lossy(&out.stdout);
            let hits: Vec<_> = tree
                .lines()
                .enumerate()
                .filter(|(_, l)| names_a_bevy_crate(l))
                .collect();
            if hits.is_empty() {
                return Ok(());
            }
            for (i, l) in hits {
                println!("{}:{l}", i + 1);
            }
            eprintln!("FAIL: a bevy crate reached the Bevy-free facade.");
            Err(1)
        }
        Step::Gate(g) => {
            let report = g.run(&ctx.root);
            for l in &report.out {
                println!("{l}");
            }
            if report.passed() { Ok(()) } else { Err(1) }
        }
        Step::DocFeatures => doc_features(ctx),
        Step::RequireCargoDeny => {
            let ok = process::Command::new("cargo")
                .args(["deny", "--version"])
                .stdout(process::Stdio::null())
                .stderr(process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                Ok(())
            } else {
                eprintln!(
                    "cargo-deny is not installed; run: cargo install cargo-deny --version 0.20.2 --locked"
                );
                Err(1)
            }
        }
    }
}

/// `cargo doc` for each feature, every one attempted; the failures are named
/// together at the end.
fn doc_features(ctx: &Ctx) -> Result<(), u8> {
    let tree = gates::RealTree {
        root: ctx.root.clone(),
    };
    let (runs, moved) = gates::doc_runs(&tree);
    let flags = std::env::var("RUSTDOCFLAGS").ok();
    let mut failed = Vec::new();
    for r in &runs {
        println!("{}", r.heading());
        let c = r.command(flags.as_deref());
        match command(&c, ctx).status() {
            Ok(s) if s.success() => {}
            Ok(_) => failed.push(r.label()),
            Err(e) => return Err(spawn_failed(&c, &e)),
        }
    }
    if let Some(e) = moved {
        eprintln!("{e}");
        return Err(1);
    }
    if failed.is_empty() {
        return Ok(());
    }
    eprintln!("FAIL: rustdoc failed for: {}", failed.join(" "));
    Err(1)
}

fn command(c: &Cmd, ctx: &Ctx) -> process::Command {
    let mut p = process::Command::new(&c.program);
    p.current_dir(&ctx.root);
    for (k, v) in &c.env {
        p.env(k, v);
    }
    if c.glob {
        p.args(c.args.iter().flat_map(|a| expand(a, &ctx.root)));
    } else {
        p.args(&c.args);
    }
    p
}

fn run(c: &Cmd, ctx: &Ctx) -> Result<(), u8> {
    let status = command(c, ctx).status().map_err(|e| spawn_failed(c, &e))?;
    if status.success() {
        return Ok(());
    }
    eprintln!("xtask: `{}` failed ({status})", c.display());
    Err(status
        .code()
        .and_then(|n| u8::try_from(n).ok())
        .unwrap_or(1))
}

fn spawn_failed(c: &Cmd, e: &std::io::Error) -> u8 {
    eprintln!("xtask: cannot run `{}`: {e}", c.program);
    1
}

/// The shell's `dir/prefix*suffix`: the sorted matches, or the pattern itself
/// when nothing matches (as `sh` leaves it).
fn expand(arg: &str, root: &Path) -> Vec<String> {
    let Some((dir, file)) = arg.rsplit_once('/') else {
        return vec![arg.to_owned()];
    };
    let Some((prefix, suffix)) = file.split_once('*') else {
        return vec![arg.to_owned()];
    };
    let mut hits: Vec<String> = std::fs::read_dir(PathBuf::from(root).join(dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| {
            n.len() >= prefix.len() + suffix.len()
                && n.starts_with(prefix)
                && n.ends_with(suffix)
                && !(n.starts_with('.') && !prefix.starts_with('.'))
        })
        .map(|n| format!("{dir}/{n}"))
        .collect();
    hits.sort();
    if hits.is_empty() {
        vec![arg.to_owned()]
    } else {
        hits
    }
}

/// `grep -E '\bbevy(_[a-z]+)?\b'`: `bevy` or `bevy_` and lowercase letters, as a whole
/// word. Backtracking never helps here: if the optional group is followed by
/// a word character, so is every shorter match.
fn names_a_bevy_crate(line: &str) -> bool {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let b = line.as_bytes();
    line.match_indices("bevy").any(|(i, _)| {
        if i > 0 && word(b[i - 1]) {
            return false;
        }
        let mut j = i + 4;
        if b.get(j) == Some(&b'_') {
            let k = j
                + 1
                + b[j + 1..]
                    .iter()
                    .take_while(|c| c.is_ascii_lowercase())
                    .count();
            if k > j + 1 {
                j = k;
            }
        }
        b.get(j).is_none_or(|&c| !word(c))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn no_recipe_or_help_is_help() {
        assert_eq!(parse(&argv(&[])), Ok(Action::Help));
        assert_eq!(parse(&argv(&["help"])), Ok(Action::Help));
        assert_eq!(parse(&argv(&["--help"])), Ok(Action::Help));
    }

    #[test]
    fn dry_run_is_read_before_the_recipe_only() {
        assert_eq!(
            parse(&argv(&["--dry-run", "bench", "--", "--dry-run"])),
            Ok(Action::Run {
                dry_run: true,
                recipe: "bench".into(),
                args: argv(&["--", "--dry-run"]),
            })
        );
        assert_eq!(
            parse(&argv(&["bench", "--dry-run"])),
            Ok(Action::Run {
                dry_run: false,
                recipe: "bench".into(),
                args: argv(&["--dry-run"])
            })
        );
    }

    #[test]
    fn an_unknown_option_is_refused() {
        assert!(parse(&argv(&["--nope", "ci"])).is_err());
    }

    #[test]
    fn help_lists_every_recipe() {
        let h = help();
        for (name, ..) in RECIPES {
            assert!(h.contains(&format!("  {name} ")), "{name}");
        }
    }

    #[test]
    fn the_bevy_pattern_is_the_greps() {
        for hit in [
            "bevy v0.19",
            "├── bevy_ecs v0.19",
            " bevy_tasks",
            "bevy",
            "tutti-bevy",
        ] {
            assert!(names_a_bevy_crate(hit), "{hit}");
        }
        for miss in [
            "bevyx",
            "bevy_ecs2",
            "bevy_Ecs",
            "bevy_",
            "xbevy",
            "debevy_ecs",
        ] {
            assert!(!names_a_bevy_crate(miss), "{miss}");
        }
    }

    #[test]
    fn a_glob_without_matches_stays_literal() {
        let dir = std::env::temp_dir().join("xtask-glob-test-none");
        assert_eq!(expand("deps/engine-*", &dir), ["deps/engine-*"]);
    }
}
