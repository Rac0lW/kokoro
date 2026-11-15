#!/usr/bin/env bash
# Build the desktop bundle, signed, and reveal it in Finder.
#
#   ./scripts/build-bundle.sh              # release bundle, signed (slow)
#   ./scripts/build-bundle.sh --debug      # debug bundle, signed (fast, incremental)
#   ./scripts/build-bundle.sh --no-sign    # skip signing (keychain will prompt again)
#   ./scripts/build-bundle.sh --identity "Developer ID Application: … (TEAMID)"
#
# The bundle lands in target/<profile>/bundle/macos/Kokoro.app
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
app_crate=$repo_root/crates/koharu

profile=release
identity=${KOHARU_SIGNING_IDENTITY:-}
for arg in "$@"; do
	case $arg in
	--debug) profile=debug ;;
	--no-sign) identity=none ;;
	--identity)
		shift
		identity=${1:?--identity needs a value}
		;;
	-h | --help)
		sed -n '2,/^[^#]/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'
		exit 0
		;;
	*)
		echo "unknown argument: $arg" >&2
		exit 2
		;;
	esac
done

bundle=$repo_root/target/$profile/bundle/macos/Kokoro.app

command -v cargo >/dev/null || {
	echo "cargo is not on PATH" >&2
	exit 1
}
cargo tauri --version >/dev/null 2>&1 || {
	echo "cargo-tauri is missing; see the postinstall script in package.json" >&2
	exit 1
}

# A stable signing identity is what keeps the login keychain from asking for the
# stored provider credentials again after every rebuild: the item's ACL is
# matched against the signature's designated requirement, not the binary.
if [ -z "$identity" ]; then
	available=$(security find-identity -v -p codesigning 2>/dev/null |
		sed -n 's/^ *[0-9]*) [0-9A-F]* "\(.*\)"$/\1/p')
	identity=$(printf '%s\n' "$available" | grep -m1 '^Developer ID Application' || true)
	[ -n "$identity" ] || identity=$(printf '%s\n' "$available" | grep -m1 '^Apple Development' || true)
fi

if [ "$identity" = none ]; then
	echo "▸ no signing identity; the bundle will be ad-hoc signed"
	unset APPLE_SIGNING_IDENTITY || true
	identity=
else
	if [ -z "$identity" ]; then
		echo "no codesigning identity found; run with --no-sign to build ad-hoc" >&2
		security find-identity -v -p codesigning >&2 || true
		exit 1
	fi
	echo "▸ signing identity: $identity"
	export APPLE_SIGNING_IDENTITY=$identity
fi

# The bundler embeds external binaries named for the target triple.
triple=$(rustc -vV | sed -n 's/^host: //p')
echo "▸ building the MCP transport"
if [ "$profile" = debug ]; then
	cargo build -p kokoro-mcp
else
	cargo build -p kokoro-mcp --release
fi
mkdir -p "$app_crate/binaries"
cp "$repo_root/target/$profile/kokoro-mcp" "$app_crate/binaries/kokoro-mcp-$triple"

echo "▸ building $profile bundle"
build_args=(build --bundles app)
if [ "$profile" = debug ]; then build_args+=(--debug); fi
# Local builds have no updater private key; without this the bundler fails after
# the .app is already signed.
build_args+=(--config '{"bundle":{"createUpdaterArtifacts":false}}')
(cd "$app_crate" && cargo tauri "${build_args[@]}")

[ -d "$bundle" ] || {
	echo "missing bundle: $bundle" >&2
	exit 1
}

echo "✓ $bundle"
codesign -dvv "$bundle" 2>&1 | grep -E '^(Authority|Signature|TeamIdentifier|Identifier)=' || true
if codesign -dv "$bundle" 2>&1 | grep -q '^Signature=adhoc'; then
	echo "⚠ ad-hoc signed: the keychain will ask again after the next rebuild" >&2
fi
open -R "$bundle"
