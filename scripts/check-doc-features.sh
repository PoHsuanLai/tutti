#!/usr/bin/env bash
#
# Rustdoc, one feature at a time, for the crates whose public surface is
# feature-gated.
#
# `cargo doc --workspace` builds every crate with its default features, which
# for these three is none, so it documents almost nothing of them; the
# `--features full,plugin` line beside it sees the union but not a link that
# only resolves with two features on. A doc comment under `#[cfg(feature =
# "x")]` is compiled by rustdoc only when `x` is on, so a link out of it into
# an item another feature gates breaks for exactly the consumer who turns on
# `x` alone.
#
# The feature list is read from each Cargo.toml, so a new feature is covered
# without editing this file. Run from the repo root; `RUSTDOCFLAGS` defaults to
# `-D warnings`. `jack` is skipped: it links libjack, as in `cargo xtask check-features`.
set -euo pipefail
cd "$(dirname "$0")/.."

export RUSTDOCFLAGS="${RUSTDOCFLAGS:--D warnings}"
crates=(crates/bevy-tutti crates/tutti-session crates/tutti)
failed=()

for dir in "${crates[@]}"; do
  if [ ! -f "$dir/Cargo.toml" ]; then
    echo "FAIL: $dir/Cargo.toml does not exist; was the crate moved?" >&2
    exit 1
  fi
  name=$(sed -n 's/^name = "\(.*\)"/\1/p' "$dir/Cargo.toml" | head -1)
  features=$(awk '/^\[features\]/{p=1;next} /^\[/{p=0} p && /^[a-z0-9_-]+ *=/{print $1}' \
    "$dir/Cargo.toml" | grep -vx -e default -e jack || true)
  for f in $features; do
    echo "== cargo doc -p $name --features $f"
    cargo doc --locked -p "$name" --no-deps --features "$f" || failed+=("$name/$f")
  done
done

if [ ${#failed[@]} -gt 0 ]; then
  echo "FAIL: rustdoc failed for: ${failed[*]}" >&2
  exit 1
fi
