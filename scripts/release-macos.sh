#!/usr/bin/env bash
# Build, sign, notarize and publish the iroh-do-chat client for macOS.
#
#   scripts/release-macos.sh VERSION          full release (`just release VERSION`)
#   scripts/release-macos.sh VERSION --dry    build, sign and package only (`just release-dry VERSION`)
#
# The release is built on this Mac with the Developer ID identity in the
# keychain; no certificate or credential leaves the machine. Output goes to
# dist/ (gitignored):
#
#   dist/iroh-do-chat-VERSION-macos-universal.zip   the signed universal binary, README.txt, LICENSE
#   dist/SHA256SUMS
#
# Environment:
#   IROH_DO_CODESIGN_IDENTITY  codesign identity (default: the first "Developer ID Application")
#   NOTARY_PROFILE             notarytool keychain profile (default: iroh-do), created once with
#                              xcrun notarytool store-credentials iroh-do --apple-id <id> --team-id <team>
#
# A bare Mach-O binary cannot have a notarization ticket stapled to it.
# Gatekeeper looks the ticket up online the first time the binary runs.
set -euo pipefail

die() { echo "release: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

[ $# -ge 1 ] || die "usage: $0 VERSION [--dry]"
version="$1"
dry=false
[ "${2:-}" = "--dry" ] && dry=true
tag="v$version"
name="iroh-do-chat"
bundle="$name-$version-macos-universal"
profile="${NOTARY_PROFILE:-iroh-do}"

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
dist="$root/dist"

step "preflight"
[ "$(uname -s)" = "Darwin" ] || die "releases are built on macOS"
[ -f LICENSE ] || die "LICENSE is missing at the repository root"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] \
    || die "VERSION must be semver without a leading v, e.g. 0.1.0 (got $version)"

crate_version=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/iroh-do-demo/Cargo.toml | head -1)
[ "$crate_version" = "$version" ] || die "crates/iroh-do-demo/Cargo.toml has version = \"$crate_version\"; set it to version = \"$version\", commit, and rerun"

ident="${IROH_DO_CODESIGN_IDENTITY:-}"
if [ -z "$ident" ]; then
    ident=$(security find-identity -v -p codesigning 2>/dev/null \
        | awk -F'"' '/"Developer ID Application:/ { print $2; exit }')
fi
[ -n "$ident" ] || die "no Developer ID Application identity in the keychain (or set IROH_DO_CODESIGN_IDENTITY)"
echo "identity: $ident"

if $dry; then
    echo "dry run: skipping git, tag, GitHub and notary checks"
else
    [ -z "$(git status --porcelain)" ] || die "the working tree is not clean; commit or stash first"
    [ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || die "releases are cut from main"
    git -c credential.https://github.com.username= fetch -q origin main
    [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] \
        || die "main is not the same commit as origin/main; push or pull first"
    git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "tag $tag already exists locally"
    git -c credential.https://github.com.username= ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1 \
        && die "tag $tag already exists on origin"
    gh auth status >/dev/null 2>&1 || die "gh is not authenticated; run gh auth login"
    xcrun notarytool history --keychain-profile "$profile" >/dev/null 2>&1 \
        || die "no usable notarytool profile \"$profile\"; create it with: xcrun notarytool store-credentials $profile --apple-id <apple-id> --team-id <team-id>"
fi

step "build (aarch64 and x86_64)"
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --locked -p iroh-do-demo --target "$target"
done
rm -rf "$dist"
mkdir -p "$dist/$bundle"
bin="$dist/$bundle/$name"
lipo -create -output "$bin" \
    "target/aarch64-apple-darwin/release/$name" \
    "target/x86_64-apple-darwin/release/$name"
lipo -info "$bin"
reported=$("$bin" --version)
[ "$reported" = "$name $version" ] || die "binary reports \"$reported\", expected \"$name $version\""

step "sign"
# Notarization requires the hardened runtime and a secure timestamp.
codesign --force --options runtime --timestamp --identifier dev.iroh-do.chat --sign "$ident" "$bin"
codesign --verify --strict --verbose=2 "$bin"
sig=$(codesign -dv "$bin" 2>&1)
[[ "$sig" == *"(runtime)"* ]] || die "the hardened runtime flag is not set"

step "package"
cat > "$dist/$bundle/README.txt" <<EOF
iroh-do-chat $version

A chat client for iroh endpoints hosted in Cloudflare Durable Objects.
It is a stock iroh 1.4 peer; each room is one endpoint, named by its
EndpointId.

  ./iroh-do-chat <E> --nick <name>      join room E
  ./iroh-do-chat --help                 all commands

Move iroh-do-chat to a directory on your PATH (for example /usr/local/bin
or ~/bin) to run it from anywhere. The binary is universal (Apple silicon
and Intel), signed with a Developer ID and notarized by Apple.

MIT licensed; see LICENSE.

Source and documentation: https://github.com/gotwalt/iroh-durable-objects
EOF
cp LICENSE "$dist/$bundle/LICENSE"
zip="$dist/$bundle.zip"
# The signature is embedded in the Mach-O; extended attributes would only add
# AppleDouble ._ files to the archive.
ditto -c -k --keepParent --norsrc --noextattr --noqtn "$dist/$bundle" "$zip"
unzip -l "$zip"

if $dry; then
    (cd "$dist" && shasum -a 256 "$bundle.zip" > SHA256SUMS)
    cat "$dist/SHA256SUMS"
    echo
    echo "dry run complete: $zip (signed, not notarized)"
    exit 0
fi

step "notarize"
result=$(xcrun notarytool submit "$zip" --keychain-profile "$profile" --wait --output-format json)
status=$(printf '%s' "$result" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get("status",""))')
id=$(printf '%s' "$result" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')
echo "submission $id: $status"
if [ "$status" != "Accepted" ]; then
    [ -n "$id" ] && xcrun notarytool log "$id" --keychain-profile "$profile" || true
    die "notarization was not accepted"
fi

step "check"
# A bare Mach-O binary cannot carry a stapled ticket, so `syspolicy_check
# distribution` always fails on it. Gatekeeper's install assessment looks the
# ticket up online and is what a user's machine does on first run.
assessment=$(spctl -a -vv -t install "$bin" 2>&1) || true
echo "$assessment"
grep -q "source=Notarized Developer ID" <<<"$assessment" \
    || die "Gatekeeper does not report the binary as notarized"

(cd "$dist" && shasum -a 256 "$bundle.zip" > SHA256SUMS)
cat "$dist/SHA256SUMS"

step "tag and publish"
notes="$dist/release-notes.md"
cat > "$notes" <<EOF
iroh-do-chat $version for macOS: a universal binary (Apple silicon and Intel), signed with a Developer ID and notarized by Apple.

Install:

\`\`\`sh
curl -LO https://github.com/gotwalt/iroh-durable-objects/releases/download/$tag/$bundle.zip
unzip $bundle.zip
mv $bundle/iroh-do-chat /usr/local/bin/    # or any directory on your PATH
\`\`\`

Join a room with \`iroh-do-chat <E> --nick <name>\`, where \`E\` is the room's EndpointId.

Check the download against \`SHA256SUMS\`: \`shasum -a 256 -c SHA256SUMS\`.

MIT licensed. The zip contains the binary, \`README.txt\` and \`LICENSE\`.
EOF
git tag -a "$tag" -m "$name $version"
git -c credential.https://github.com.username= push origin "$tag"
gh release create "$tag" "$zip" "$dist/SHA256SUMS" \
    --verify-tag --title "$name $version" --notes-file "$notes"
echo
echo "released $tag"
