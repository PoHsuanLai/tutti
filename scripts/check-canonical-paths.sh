#!/usr/bin/env bash
#
# Engine types are imported from a crate's root or prelude, never through its
# modules. Neither rustc nor clippy has a lint for a redundant path, and doc
# comments in ```text / ```ignore blocks are never compiled — so this grep is
# the only enforcement. Searches *.md as well as *.rs for that reason.
#
# Matches tutti-to-tutti chains only; `pub use midi2;` / `midly` are deliberate
# third-party re-exports and are left alone.
#
# Usage:
#   scripts/check-canonical-paths.sh                       # chained-path gate
#   scripts/check-canonical-paths.sh tutti_rt cell vec      # + per-module gate

set -uo pipefail

cd "$(dirname "$0")/.."

# _archive/ is excluded by design: those crates are kept as SPEC for rebuilds and
# reference APIs that no longer exist. They are hidden from cargo (Cargo.toml ->
# .txt) and must not gate a build.
readonly EXCLUDES=(
  --exclude-dir=_archive
  --exclude-dir=target
  --exclude-dir=.git
)

status=0

# A moved root must fail here: grep over a missing directory finds nothing and
# the gate would pass having read nothing.
for root in crates testing; do
  if [ -z "$(find "$root" -name '*.rs' -print -quit 2>/dev/null)" ]; then
    echo "FAIL: $root/ holds no .rs file; was the layout changed?"
    exit 1
  fi
done

echo "==> chained tutti_X::tutti_Y:: paths"
# Matches a tutti crate reached THROUGH another tutti crate. The trailing `::`
# is required so a bare `pub use tutti_types;` (the declaration being removed,
# which lives in exactly one place per crate) is not itself reported here.
if chained=$(grep -rnE "tutti_[a-z0-9_]+::tutti_[a-z0-9_]+::" \
      --include='*.rs' --include='*.md' "${EXCLUDES[@]}" crates/ testing/ 2>/dev/null); then
  echo "$chained"
  count=$(printf '%s\n' "$chained" | wc -l | tr -d ' ')
  echo "FAIL: $count chained path(s). Import from the owning crate's root instead."
  status=1
else
  echo "ok"
fi

# The facade's own rule: every whole-crate re-export in `crates/tutti` must be
# aliased.
#
# A bare `pub use tutti_engine;` there would let a consumer write
# `tutti::tutti_engine::Engine` — exactly the redundant path the chained gate
# above exists to prevent, in a shape its regex CANNOT see: it matches
# `tutti_x::tutti_y::`, and `tutti::` has no underscore segment. Aliasing
# removes the spelling from the language rather than asking anyone to avoid
# it.
#
# `crates/tutti/tests/no_logic.rs` asserts the same thing from inside the
# crate; this is here so the rule is visible to someone reading the gates
# rather than only to someone running the tests.
if [ -f crates/tutti/src/lib.rs ]; then
  echo "==> unaliased crate re-exports in the facade"
  if bare=$(grep -nE "^\s*pub use tutti_[a-z0-9_]+\s*;" crates/tutti/src/lib.rs 2>/dev/null); then
    echo "$bare"
    echo "FAIL: alias it (\`as engine\`, \`as sampler\`, ...) so tutti::tutti_x:: is unspellable."
    status=1
  else
    echo "ok"
  fi
fi

# Per-module gate. Called as: <crate_path_prefix> <mod> [<mod> ...]
# e.g. `tutti_rt cell vec` checks for tutti_rt::cell:: and tutti_rt::vec::
if [ "$#" -ge 2 ]; then
  crate="$1"; shift
  echo "==> redundant ${crate}::<module>:: paths"
  alt=$(IFS='|'; echo "$*")
  if hits=$(grep -rnE "${crate}::(${alt})::" \
        --include='*.rs' --include='*.md' "${EXCLUDES[@]}" crates/ testing/ 2>/dev/null); then
    echo "$hits"
    count=$(printf '%s\n' "$hits" | wc -l | tr -d ' ')
    echo "FAIL: $count path(s) through a privatized module. Use ${crate}::<Item>."
    status=1
  else
    echo "ok"
  fi
fi

exit "$status"
