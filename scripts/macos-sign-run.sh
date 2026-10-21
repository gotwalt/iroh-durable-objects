#!/usr/bin/env bash
# Cargo runner (and `just` wrapper): code-sign a binary, then exec it.
#
#   scripts/macos-sign-run.sh <binary> [args...]
#
# The macOS application firewall asks to accept incoming connections for each
# ad-hoc-signed binary that listens, and each rebuild has a new ad-hoc
# signature. With "Automatically allow downloaded signed software" enabled (the
# default), a binary signed with a Developer ID identity is allowed without a
# prompt.
#
# IROH_DO_CODESIGN_IDENTITY selects the identity:
#   unset   the first "Developer ID Application" identity in the keychain;
#           if there is none, the binary runs with its ad-hoc signature
#   <name>  any identity `codesign -s` accepts (name or SHA-1)
#   none    do not sign
#
# On other platforms the binary is run unchanged.
set -euo pipefail

bin="$1"; shift

if [ "$(uname -s)" = "Darwin" ] && [ "${IROH_DO_CODESIGN_IDENTITY:-}" != "none" ]; then
  ident="${IROH_DO_CODESIGN_IDENTITY:-}"
  if [ -z "$ident" ]; then
    ident=$(security find-identity -v -p codesigning 2>/dev/null \
      | awk -F'"' '/"Developer ID Application:/ { print $2; exit }')
  fi
  if [ -n "$ident" ]; then
    # Stable identifier per binary (test binaries carry a -<hash> suffix).
    name=$(basename "$bin" | sed -E 's/-[0-9a-f]{16}$//')
    if ! out=$(codesign --force --sign "$ident" --identifier "dev.iroh-do.$name" \
        --timestamp=none "$bin" 2>&1); then
      echo "macos-sign-run: codesign failed, running unsigned: $out" >&2
    fi
  fi
fi

exec "$bin" "$@"
