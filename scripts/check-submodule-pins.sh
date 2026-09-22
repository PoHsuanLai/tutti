#!/usr/bin/env bash
#
# The VST3 SDK is three Steinberg submodules, and this asserts each one is at
# the commit the repo documents as the `v3.8.0_build_66` release. Three places
# state that pin in prose — `.gitmodules`, `CLAUDE.md`, and the `VENDORED_SDK`
# doc comment in `crates/plugin/formats/tutti-vst3-host/build.rs` — and prose
# cannot notice when a submodule bump moves the tree out from under it. This is
# the only check that can.
#
# It asserts SHAs rather than `git describe` output, and that choice is the
# whole reason the script is not a one-line grep for the tag name. In a normal
# clone `git describe` reports `v3.7.3_build_20-10-g3d2e82f` and two like it,
# naming a release these trees are well past: a submodule fetch brings the
# commit its superproject records, not every tag that happens to point near it,
# so the 3.8.0 tag was never fetched here and describe falls back to the
# nearest ANCESTOR tag it does have. The checked-out content really is 3.8.0 —
# `LICENSE.txt` in all three is the MIT relicensing that shipped with that
# release ("Copyright (c) 2025, Steinberg Media Technologies GmbH"), and
# `docs/reference/vst3-3.8.0-interface-surface.md` corroborates it against the
# headers. A gate built on describe would therefore fail loudly on a perfectly
# correct tree, and a gate that cries wolf is a gate somebody deletes. A SHA
# cannot be wrong about which commit is checked out, and it is what the
# superproject actually records, so that is what this compares.
#
# Usage:
#   scripts/check-submodule-pins.sh

set -uo pipefail

cd "$(dirname "$0")/.."

# The `v3.8.0_build_66` commits, one `<path> <sha>` pair per submodule. Bumping
# the SDK means editing these three lines in the same commit as the submodule
# move; that is the point of the gate, not an inconvenience it imposes.
readonly PINS=(
  "crates/plugin/vendor/vst3-sdk/base           3d2e82f8e6bff59c1d8b7a27491a29c2286b5206"
  "crates/plugin/vendor/vst3-sdk/pluginterfaces 31d6eeba6daaa3e2a8bfbe3e7a90ca0b7fbfbc1c"
  "crates/plugin/vendor/vst3-sdk/public.sdk     a3911a4615dabbfdfd9d181ee26b05c70c289a95"
)

status=0

echo "==> VST3 SDK submodule pins (v3.8.0_build_66)"

for pin in "${PINS[@]}"; do
  path=${pin%% *}
  want=${pin##* }

  line=$(git submodule status -- "$path" 2>/dev/null)
  if [ -z "$line" ]; then
    echo "FAIL $path"
    echo "     git knows no submodule at this path. It has been renamed or"
    echo "     dropped from .gitmodules; fix the path here and in the three"
    echo "     places that document the pin in prose."
    status=1
    continue
  fi

  # `git submodule status` prefixes the SHA with exactly one status character:
  # a space when the working tree sits at the recorded commit, `-` when the
  # submodule is not initialised, `+` when it is checked out at some other
  # commit, and `U` during a merge conflict. Stripping it is not cosmetic —
  # leaving it on compares a 41-character string against a 40-character one, so
  # every entry would report a mismatch, and for a reason the output would not
  # explain.
  flag=${line:0:1}
  have=$(printf '%s' "${line:1}" | cut -d' ' -f1)

  case "$flag" in
    -)
      # Distinguished from a mismatch deliberately: an uninitialised submodule
      # still prints the SHA the superproject records, so a naive comparison
      # would PASS on an empty directory and report a healthy SDK that is not
      # on disk at all. Say what is actually wrong and how to fix it.
      echo "FAIL $path"
      echo "     submodule not initialised — the directory is empty, so there"
      echo "     is no SDK here to check and tutti-vst3-host cannot build its"
      echo "     audio-probe reference plugin."
      echo "     Run: git submodule update --init --recursive"
      status=1
      continue
      ;;
    U)
      echo "FAIL $path"
      echo "     submodule has a merge conflict; resolve it before the pin can"
      echo "     be checked."
      status=1
      continue
      ;;
    +)
      # The SHA printed after `+` is the one checked out, not the one recorded
      # in the index, so the two disagree whatever the comparison below would
      # say. Committing from here would move the pin as a side effect of
      # whatever left the submodule detached.
      echo "FAIL $path"
      echo "     working tree is at $have, which is not the commit the"
      echo "     superproject records. Run: git submodule update --recursive"
      status=1
      continue
      ;;
  esac

  if [ "$have" != "$want" ]; then
    echo "FAIL $path"
    echo "     expected $want"
    echo "     found    $have"
    echo "     If the SDK was bumped on purpose, update this script and the"
    echo "     three prose mentions (.gitmodules, CLAUDE.md, build.rs) too."
    status=1
  else
    echo "ok   $path"
  fi
done

if [ "$status" -ne 0 ]; then
  echo "FAIL: the vendored VST3 SDK is not at the documented v3.8.0_build_66 pin."
fi

exit "$status"
