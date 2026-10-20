#!/usr/bin/env bash
#
# `tutti-session` is the Bevy-free API layer both umbrellas use (design 015
# §8, checks 2 and 4). Three assertions, none of which a compile can make:
#
# 1. No Bevy crate in its normal dependency closure, with every feature on.
#    `cargo tree`, not `cargo metadata`: metadata resolves features once for
#    the whole workspace, so `bevy-tutti`'s Bevy features leak into every
#    member's closure. `--all-features` is required: a Bevy crate behind an
#    optional feature is absent from the default tree. `-e normal`, because
#    `bevy-tutti` is a workspace member and a dev-dependency walk would reach
#    it.
# 2. No `bevy` feature in its manifest. Every other engine crate has one,
#    off by default; this crate must never grow one, which is a stronger rule
#    than (1) can state while the feature has no dependency behind it yet.
# 3. No whole-crate re-export in its source: `pub use tutti_x as ...;`,
#    `pub use tutti_x;` or `pub use tutti_x::*;`. Single items are allowed
#    (the types its own signatures take). A crate re-exported whole makes it
#    a third umbrella, and the umbrella is `tutti`.
#
# A script rather than a test: a test that shells out to `cargo` inside
# `cargo nextest` contends for the build lock.
#
# Mutations (run when this script landed, each one then reverted):
# - `bevy_ecs` as an optional dependency of `tutti-session` behind an `ecs`
#   feature → (1) lists `bevy_ecs` and fails; the same tree without
#   `--all-features` lists nothing, which is why the flag is there.
# - `bevy = []` under `[features]` → (2) fails.
# - `pub use tutti_mod as modulation;` in `lib.rs` → (3) fails and names
#   `tutti` as the umbrella; `pub use tutti_mod;` and `pub use tutti_mod::*;`
#   fail the same way.
#
# Usage:
#   scripts/check-bevy-free.sh

set -uo pipefail

cd "$(dirname "$0")/.."

readonly CRATE=tutti-session
readonly MANIFEST=crates/tutti-session/Cargo.toml
readonly SRC=crates/tutti-session/src

fail=0

# 1. The dependency closure.
if ! tree=$(cargo tree -p "$CRATE" -e normal --all-features --prefix none 2>&1); then
  echo "$tree"
  echo "FAIL: cargo tree -p $CRATE did not run."
  exit 1
fi
if bevy=$(grep -E '^bevy([_ ]|$)' <<<"$tree" | sort -u) && [[ -n $bevy ]]; then
  echo "$bevy"
  echo "FAIL: a Bevy crate is in $CRATE's normal dependency closure (with --all-features)."
  echo "      $CRATE is the Bevy-free API layer. Bevy code goes in bevy-tutti, which"
  echo "      depends on $CRATE, never the reverse."
  fail=1
fi

# 2. The manifest's features. A `bevy` key in the `[features]` table.
if awk '
  /^\[/ { in_features = ($0 ~ /^\[features\][[:space:]]*$/) ; next }
  in_features && /^[[:space:]]*"?bevy"?[[:space:]]*=/ { found = 1; print FILENAME ":" NR ": " $0 }
  END { exit !found }
' "$MANIFEST"; then
  echo "FAIL: $MANIFEST declares a \`bevy\` feature."
  echo "      $CRATE has none and must never have one: the adapter is bevy-tutti."
  fail=1
fi

# 3. Whole-crate re-exports. Comment lines never start with `pub`, so they
#    cannot match.
readonly WHOLE='^[[:space:]]*pub use (::)?tutti_[a-z0-9_]+([[:space:]]+as[[:space:]]+[A-Za-z0-9_]+)?[[:space:]]*;|^[[:space:]]*pub use (::)?tutti_[a-z0-9_]+::\*[[:space:]]*;'
if hits=$(grep -rnE "$WHOLE" "$SRC") && [[ -n $hits ]]; then
  echo "$hits"
  echo "FAIL: $CRATE re-exports a crate whole. It is not an umbrella: the umbrella is"
  echo "      \`tutti\` (and \`bevy-tutti\` for a Bevy app). Re-export the single items"
  echo "      $CRATE's own signatures take, or let the umbrella re-export the crate."
  fail=1
fi

if [[ $fail -ne 0 ]]; then
  exit 1
fi
echo "OK: $CRATE is Bevy-free, has no bevy feature, and re-exports no crate whole."
