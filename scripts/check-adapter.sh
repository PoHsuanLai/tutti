#!/usr/bin/env bash
#
# The two adapter rules of design 015 §8 (checks 3 and 4), which no compile
# can make:
#
# 3. `bevy-tutti` holds no engine composition. Its source
#    (`crates/bevy-tutti/src`), tests and comments excluded, names none of the
#    types in DENY below as a whole word. Each is a piece of composition the
#    session owns; the entry names the owner, and a hit prints it. Method
#    names (`.commit()`, `.apply(`) are not checked: they cannot tell an
#    `Editor` from a `Session`. A type name can, and no composition happens
#    without naming one.
# 4. `tutti-session` is not an umbrella. Its source re-exports no crate (or
#    module) whole: `pub use tutti_x as ...;`, `pub use tutti_x;`,
#    `pub use tutti_x::{self ...}`, or a glob anywhere under a `tutti_` path
#    (`pub use tutti_x::*;`, `pub use tutti_x::prelude::*;`,
#    `pub use tutti_x::{a::*, B};`), with or without attributes in front,
#    across lines, in groups nested to any depth
#    (`pub use tutti_x::{a::{B}, self};`) and in a root group
#    (`pub use {tutti_x as m};`). Single items are allowed: the types its own signatures
#    take. (This check moved here from `check-bevy-free.sh`, which ran it
#    from PR 4 to PR 7; its mutation records are below.)
#
# A script rather than a test: a test that shells out inside `cargo nextest`
# contends for the build lock, and this needs no build at all (perl only), so
# CI runs it in the static `paths` job.
#
# How (3) reads a file. A small lexer, not a grep, so that:
# - comments (`//`, `///`, `//!`, `/* */`, nested) and the contents of string,
#   raw-string, byte-string and char literals are blanked, newlines kept (so
#   line numbers hold, and `error!("... Audio Engine ...")` is not a hit);
# - an item under `#[cfg(test)]` or `#[cfg(all(test, ...))]` is blanked from
#   the attribute to the end of what it governs: the `}` that closes its
#   first brace (a `mod tests {}` block, a test-only `fn`, anywhere in the
#   file), a `;` or `,` at depth 0 (an out-of-line `mod`, a `use`, a struct
#   or tuple field, a call argument, an array element), or the closer of the
#   group it sits in (a last field or argument without a trailing comma),
#   which is kept;
# - a test module declared out of line (`#[cfg(test)] mod tests;`, with or
#   without `#[path = ".."]`) excludes its file, and any directory module of
#   that name, by path;
# - a session phase (`Phase::Modulation`) is the session's enum variant, not
#   the type of that name, and is not a hit.
#
# Its limits, each a way to hide a name from it, none a way to report a
# false hit on code that names none:
# - `#[cfg(any(test, ..))]` and `#[cfg(not(..))]` items are checked (they
#   build outside tests); so is test-only code without a `cfg(test)` on the
#   item itself (a `pub mod test_support`), deliberately.
# - A name built by a macro from pieces (`concat_idents!`, a `paste!` of
#   `Edi` and `tor`), or reached through a type alias declared outside
#   `bevy-tutti/src`, is not seen. An alias declared inside it is: the
#   alias names the type.
# - A `,` at depth 0 ends the blanked span, and `<`/`>` are not brackets
#   (they are also comparisons), so a test-only item whose header has a
#   comma outside parentheses (`fn f<A, B>()`, `impl<T> X where T: A, U: B`)
#   is blanked only up to that comma. That can only report a false hit on
#   the rest of the item, never hide one; split the item if it happens.
# - The cfg attribute must be on its own item; an inner `#![cfg(test)]` at
#   the top of a module file is not read (`bevy-tutti` has none: its test
#   modules are `#[cfg(test)] mod tests` at the end of a file, or out of line
#   in `tests.rs`).
#
# On landing (PR 7) it found one hit: `bevy_tutti::export` re-exported
# `RenderGraph`. The re-export is gone; a prepare hook calls the graph's
# methods without naming it, and a host that names it depends on
# `tutti-export`.
#
# Mutations (run when this script landed, each one then reverted):
# - `fn f(e: &mut tutti_graph::Editor) { e.commit(); }` in
#   `src/graph/commit.rs` → (3) fails, names `Editor` at that line and
#   `tutti_session::Session` (its `Graph`) as the owner.
# - `pub use tutti_mod as modulation;` in `tutti-session`'s `lib.rs` → (4)
#   fails and names `tutti` as the umbrella; `pub use tutti_mod;` and
#   `pub use tutti_mod::*;` fail the same way.
# - (4), from `check-bevy-free.sh`: `#[cfg(feature = "modulation")] pub use
#   tutti_mod as modulation;` (an attribute on the same line), `pub use
#   tutti_mod::{self as modulation};` and `pub use tutti_core::prelude::*;`
#   each fail (PR 4 review).
# - The lexer, each case appended to `src/graph/commit.rs`: `Engine` in a
#   `//` comment, in nested `/* */`, in a string and a raw string, inside
#   `#[cfg(test)] mod tests { .. }` (with `"}"` and `'}'` in it) and as
#   `Phase::Modulation` passes; `tutti_core::Engine` on an item after that
#   block, after a `#[cfg(test)]` fn, and under `#[cfg(any(test, ..))]`
#   fails. `type E = tutti_core::Engine;` in the out-of-line
#   `src/graph/tests.rs` passes, and fails with the by-path exclusion
#   deleted.
#   (Current main names no denied type inside its test code either, so the
#   exclusions are exercised only by these cases.)
# - (3), PR 7 review: a `#[cfg(test)]` on a field, an argument or an
#   expression used to blank the rest of the file (the scan went past the
#   closer of its group at depth -1). `AudioGraphRes`'s `cfg(test)` phase
#   log field hid the 247 lines after it, and
#   `let _probe: Option<tutti_core::Engine> = None;` planted in its `read()`
#   passed. With the scan ending at a depth-0 `,` or the closer of its
#   group, each of these, appended to `src/graph/commit.rs` with
#   `fn k() -> Option<tutti_core::Engine> { None }` after it, now fails (and
#   passed before): `struct P(u8, #[cfg(test)] u16);`,
#   `struct Q { a: u8, #[cfg(test)] b: u16 }` (a last field, no comma),
#   `(x, #[cfg(test)] x)` in a fn body, and
#   `g(1, #[cfg(test)] 2, 3); let _v = [1, #[cfg(test)] 2];`. The planted
#   probe in `read()` fails too. `struct P(u8, #[cfg(test)]
#   tutti_core::Engine);` passes (the test-only field is still excluded),
#   and `#[cfg(test)] fn t<A, B>() -> Option<tutti_core::Engine>` reports
#   a false hit (the generics limit above).
# - (4), PR 7 review: `pub use tutti_mod::{a::{B}, self};` and
#   `pub use {tutti_mod as m};` passed the old regex. Check 4 now parses the
#   use tree, and both fail, as do `pub use ::tutti_mod::{x::{y::{*}}};` and
#   a multi-line `pub use tutti_mod::{ a::{B, C}, self as m, };`. These
#   pass: `pub use tutti_mod::{a::{B}, C};`, `pub use {tutti_mod::A as M};`,
#   `pub(crate) use tutti_mod;`, and `pub use tutti_mod;` in a comment or
#   a string. Every earlier (4) mutation above still fails.
#
# Usage:
#   scripts/check-adapter.sh

set -uo pipefail

cd "$(dirname "$0")/.."

readonly ADAPTER_SRC=crates/bevy-tutti/src
readonly SESSION_SRC=crates/tutti-session/src

# DENY: `type|owner, and what to call instead`. The owner is printed on a hit.
readonly deny_list='
Editor|tutti_session::Session and its Graph (the graph'"'"'s control half): Session::graph(), Graph::commit
Executor|tutti_session::Session and its Graph (the graph'"'"'s audio half): SessionBuilder, Graph::take_audio_side
Modulation|the session'"'"'s modulation phase: Session::run_phase(Phase::Modulation, ..) with a ModInput
TuttiDriver|tutti_session::Device (the sound card): SessionBuilder::build_device, Session::restart
AudioCallbackState|tutti_session::Device (the sound card): SessionBuilder::build_device
Engine|tutti_session::SessionBuilder (the renderer): build_device / build_offline
RenderGraph|tutti_session::Session::export (the offline render): ExportRequest, ExportJob, finish_export
'

# Shared by both checks: strict perl, and `lex`, which blanks comments and
# literal contents (keeping newlines, so offsets and line numbers hold).
prelude_pl=$(cat <<'PL'
use strict; use warnings;
# Blank comments and literal contents, keeping newlines.
sub lex {
  my ($s) = @_;
  my $out = "";
  pos($s) = 0;
  my $blank = sub { (my $t = $_[0]) =~ s/[^\n]/ /g; $t };
  while (pos($s) < length $s) {
    if ($s =~ /\G(\/\/[^\n]*)/gc) { $out .= $blank->($1); next }
    if ($s =~ /\G\/\*/gc) {
      my $depth = 1; my $text = "/*";
      while ($depth && pos($s) < length $s) {
        if ($s =~ /\G\/\*/gc) { $depth++; $text .= "/*" }
        elsif ($s =~ /\G\*\//gc) { $depth--; $text .= "*/" }
        else { $s =~ /\G(.)/gcs; $text .= $1 }
      }
      $out .= $blank->($text); next
    }
    if ($s =~ /\G(b?r(#*)")/gc) {
      my ($open, $hashes) = ($1, $2);
      $s =~ /\G(.*?"$hashes)/gcs or die "unterminated raw string\n";
      $out .= $open . $blank->(substr($1, 0, -1 - length $hashes)) . "\"" . $hashes; next
    }
    if ($s =~ /\G(b?")/gc) {
      my $open = $1;
      $s =~ /\G((?:\\.|[^"\\])*)"/gcs or die "unterminated string\n";
      $out .= $open . $blank->($1) . "\""; next
    }
    if ($s =~ /\G(b?\x27(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]*\}|.)|[^\\\x27\n])\x27)/gc) {
      $out .= $blank->($1); next
    }
    if ($s =~ /\G([A-Za-z_][A-Za-z0-9_]*|.)/gcs) { $out .= $1; next }
  }
  return $out;
}
PL
)
readonly prelude_pl

fail=0

# 3. The adapter's source. Perl: one pass to collect the out-of-line test
#    modules, one to blank and scan.
hits=$(DENY="$deny_list" perl -e "$prelude_pl"'
use File::Find; use File::Basename;

my $root = shift @ARGV;
my %deny;
for (split /\n/, $ENV{DENY} // "") { next unless /\S/; my ($t, $o) = split /\|/, $_, 2; $deny{$t} = $o; }
die "the deny-list is empty\n" unless %deny;
my @files;
find({ wanted => sub { push @files, $File::Find::name if /\.rs\z/ }, no_chdir => 1 }, $root);
@files = sort @files;


# A test-only attribute: `#[cfg(test)]` or `#[cfg(all(test, ...))]`.
my $test_attr = qr/\#\[\s*cfg\s*\(\s*(?:test|all\s*\(\s*test\s*,[^\]]*\))\s*\)\s*\]/;

my (%code, %skip_file, @skip_dir);
for my $f (@files) {
  open my $fh, "<", $f or die "$f: $!\n";
  local $/; my $src = <$fh>; close $fh;
  my $c = lex($src);
  # Blank every test-only item, and note the out-of-line modules.
  while ($c =~ /$test_attr/g) {
    my $start = $-[0];
    my $i = $+[0];
    my $path;
    # Further attributes on the same item.
    while (substr($c, $i) =~ /\A(\s*\#\[)/) {
      my $at = $i + length($1) - 2;
      my $d = 0; my $j = $at;
      for (; $j < length $c; $j++) {
        my $ch = substr($c, $j, 1);
        $d++ if $ch eq "["; $d-- if $ch eq "]";
        last if $d == 0 && $ch eq "]";
      }
      my $attr = substr($src, $at, $j + 1 - $at);
      $path = $1 if $attr =~ /path\s*=\s*"([^"]*)"/;
      $i = $j + 1;
    }
    if (substr($c, $i) =~ /\A\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;/) {
      my $name = $1;
      my $dir = dirname($f);
      my $base = basename($f, ".rs");
      my $mdir = ($base =~ /\A(?:mod|lib|main)\z/) ? $dir : "$dir/$base";
      if (defined $path) { $skip_file{"$dir/$path"} = 1 }
      else { $skip_file{"$mdir/$name.rs"} = 1; push @skip_dir, "$mdir/$name/" }
    }
    # What the attribute governs: to the `}` closing its first brace, a `;`
    # or `,` at depth 0, or (kept) the closer of the enclosing group. Without
    # the last two, a cfg(test) field or argument ran on past the closer of
    # its group at depth -1 and blanked the rest of the file.
    my $d = 0; my $j = $i;
    for (; $j < length $c; $j++) {
      my $ch = substr($c, $j, 1);
      if ($ch eq "{" || $ch eq "(" || $ch eq "[") { $d++ }
      elsif ($ch eq "}" || $ch eq ")" || $ch eq "]") {
        $d--;
        if ($d < 0) { $j--; last }
        last if $d == 0 && $ch eq "}";
      }
      elsif (($ch eq ";" || $ch eq ",") && $d == 0) { last }
    }
    my $len = $j + 1 - $start;
    (my $b = substr($c, $start, $len)) =~ s/[^\n]/ /g;
    substr($c, $start, $len) = $b;
    pos($c) = $start + $len;
  }
  $code{$f} = $c;
}

my $words = join "|", map { quotemeta } sort keys %deny;
for my $f (@files) {
  next if $skip_file{$f};
  next if grep { index($f, $_) == 0 } @skip_dir;
  my @lines = split /\n/, $code{$f}, -1;
  open my $fh, "<", $f or die; my @orig = <$fh>; close $fh;
  for my $n (0 .. $#lines) {
    (my $l = $lines[$n]) =~ s/\bPhase\s*::\s*\w+//g;
    while ($l =~ /(?<![\w])($words)(?![\w])/g) {
      my $w = $1;
      (my $text = $orig[$n]) =~ s/^\s+|\s+$//g;
      printf "%s:%d: %s\n    denied `%s`; it belongs to %s\n", $f, $n + 1, $text, $w, $deny{$w};
    }
  }
}
' "$ADAPTER_SRC")
status=$?
if [[ $status -ne 0 ]]; then
  echo "$hits"
  echo "FAIL: the bevy-tutti scan did not run (perl exited $status)."
  exit 1
fi
if [[ -n $hits ]]; then
  echo "$hits"
  echo "FAIL: bevy-tutti names engine composition the session owns (design 015 §8,"
  echo "      check 3). bevy-tutti is the adapter: ECS components, systems, assets and"
  echo "      windows. Move the composition into tutti-session and call it from here."
  fail=1
fi

# 4. Whole-crate re-exports in the session. Each file is lexed (comments
#    and literals blanked), and each `pub use ... ;` is parsed as a use
#    tree, braces nested to any depth, so a statement spanning lines, a
#    group inside a group, and a root group (`pub use {..};`) are all read.
#    `pub(crate) use` is not a re-export and is not read. A leaf is a hit
#    when its full path starts at a `tutti_` crate (optionally `::`-led) and
#    it is:
#    - the crate itself, with or without `as NAME` (`tutti_x`);
#    - `self` (`tutti_x::{self}`, `tutti_x::a::{self as m}`: a crate or
#      module whole);
#    - a glob at any depth (`tutti_x::*`, `tutti_x::prelude::*`).
hits=$(find "$SESSION_SRC" -name '*.rs' -print0 | sort -z | xargs -0 perl -e "$prelude_pl"'
my @found;
# Parse one use tree from @$t at $$k under the path @$pre; record the
# whole-crate leaves.
sub tree {
  my ($t, $k, $pre, $hit) = @_;
  my @path = @$pre;
  $$k++ if $$k < @$t && $t->[$$k] eq "::";
  while ($$k < @$t) {
    my $tok = $t->[$$k];
    if ($tok eq "{") {
      $$k++;
      while ($$k < @$t && $t->[$$k] ne "}") {
        tree($t, $k, \@path, $hit);
        $$k++ if $$k < @$t && $t->[$$k] eq ",";
      }
      $$k++;
      return;
    }
    if ($tok eq "*") {
      $$k++;
      push @$hit, join("::", @path, "*") if @path && $path[0] =~ /\Atutti_/;
      return;
    }
    if ($tok =~ /\A\w+\z/) {
      $$k++;
      if ($$k < @$t && $t->[$$k] eq "::") { push @path, $tok; $$k++; next }
      my $full = join "::", @path, $tok;
      $$k += 2 if $$k < @$t && $t->[$$k] eq "as";
      if ($tok eq "self") {
        push @$hit, $full if @path && $path[0] =~ /\Atutti_/;
      } elsif (!@path && $tok =~ /\Atutti_/) {
        push @$hit, $full;
      }
      return;
    }
    $$k++;
    return;
  }
}
for my $f (@ARGV) {
  open my $fh, "<", $f or die "$f: $!\n";
  local $/; my $src = <$fh>; close $fh;
  my $c = lex($src);
  while ($c =~ /(?<![\w)])pub\s+use\b/g) {
    my $at = $-[0];
    my $body_at = $+[0];
    my $semi = index($c, ";", $body_at);
    die "$f: unterminated use\n" if $semi < 0;
    my $body = substr($c, $body_at, $semi - $body_at);
    my @t = $body =~ /(::|\w+|[{},*])/g;
    my ($k, @hit) = (0);
    tree(\@t, \$k, [], \@hit);
    next unless @hit;
    my $line = 1 + (substr($c, 0, $at) =~ tr/\n//);
    (my $text = substr($src, $at, $semi + 1 - $at)) =~ s/\s+/ /g;
    print "$f:$line: $text\n    whole: ", join(", ", @hit), "\n";
  }
}
')
if [[ -n $hits ]]; then
  echo "$hits"
  echo "FAIL: tutti-session re-exports a crate whole (design 015 §8, check 4). It is"
  echo "      not an umbrella: the umbrella is \`tutti\` (and \`bevy-tutti\` for a Bevy"
  echo "      app). Re-export the single items tutti-session's own signatures take, or"
  echo "      let the umbrella re-export the crate."
  fail=1
fi

if [[ $fail -ne 0 ]]; then
  exit 1
fi
echo "OK: bevy-tutti names no engine composition, and tutti-session re-exports no crate whole."
