#!/usr/bin/env bash
#
# `tutti-session` is the Bevy-free API layer both umbrellas use (design 015
# §8, check 2). Two assertions, neither of which a compile can make:
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
#
# Its former third check, no whole-crate re-export (§8 check 4), is
# `scripts/check-adapter.sh`'s, with its mutation records.
#
# A script rather than a test: a test that shells out to `cargo` inside
# `cargo nextest` contends for the build lock.
#
# Mutations (run when this script landed, each one then reverted):
# - `bevy_ecs` as an optional dependency of `tutti-session` behind an `ecs`
#   feature → (1) lists `bevy_ecs` and fails; the same tree without
#   `--all-features` lists nothing, which is why the flag is there.
# - `bevy = []` under `[features]` → (2) fails.
#
# Usage:
#   scripts/check-bevy-free.sh

set -uo pipefail

cd "$(dirname "$0")/.."

readonly CRATE=tutti-session
readonly MANIFEST=crates/tutti-session/Cargo.toml

fail=0

# 1. The dependency closure.
if ! tree=$(cargo tree --locked -p "$CRATE" -e normal --all-features --prefix none 2>&1); then
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

if [[ $fail -ne 0 ]]; then
  exit 1
fi
echo "OK: $CRATE is Bevy-free and has no bevy feature."
