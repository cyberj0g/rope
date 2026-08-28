#!/usr/bin/env bash
set -euo pipefail

NODE_VERSION="${NODE_VERSION:-24.20.0}"
PATCHRIGHT_VERSION="${PATCHRIGHT_VERSION:-1.62.1}"
AUTOCONSENT_VERSION="${AUTOCONSENT_VERSION:-16.31.0}"
TARGET="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT="$ROOT/browser-runtime/$TARGET.tar.gz"

case "$TARGET" in
    x86_64-unknown-linux-gnu) NODE_TARGET="linux-x64"; ARCHIVE_EXT="tar.xz"; NODE_SOURCE="bin/node"; NODE_FILE="node" ;;
    aarch64-unknown-linux-gnu) NODE_TARGET="linux-arm64"; ARCHIVE_EXT="tar.xz"; NODE_SOURCE="bin/node"; NODE_FILE="node" ;;
    x86_64-apple-darwin) NODE_TARGET="darwin-x64"; ARCHIVE_EXT="tar.gz"; NODE_SOURCE="bin/node"; NODE_FILE="node" ;;
    aarch64-apple-darwin) NODE_TARGET="darwin-arm64"; ARCHIVE_EXT="tar.gz"; NODE_SOURCE="bin/node"; NODE_FILE="node" ;;
    x86_64-pc-windows-msvc) NODE_TARGET="win-x64"; ARCHIVE_EXT="zip"; NODE_SOURCE="node.exe"; NODE_FILE="node.exe" ;;
    aarch64-pc-windows-msvc) NODE_TARGET="win-arm64"; ARCHIVE_EXT="zip"; NODE_SOURCE="node.exe"; NODE_FILE="node.exe" ;;
    *) echo "unsupported target: $TARGET" >&2; exit 1 ;;
esac

for command in curl npm; do
    command -v "$command" >/dev/null || { echo "$command is required" >&2; exit 1; }
done
if command -v python3 >/dev/null; then
    PYTHON="python3"
elif command -v python >/dev/null; then
    PYTHON="python"
else
    echo "python is required" >&2
    exit 1
fi
if [[ "$ARCHIVE_EXT" == "zip" ]]; then
    command -v unzip >/dev/null || { echo "unzip is required" >&2; exit 1; }
fi

sha256() {
    if command -v sha256sum >/dev/null; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
NODE_ARCHIVE="node-v$NODE_VERSION-$NODE_TARGET.$ARCHIVE_EXT"
BASE_URL="https://nodejs.org/dist/v$NODE_VERSION"

curl --fail --location --silent --show-error "$BASE_URL/SHASUMS256.txt" --output "$WORK/SHASUMS256.txt"
curl --fail --location --silent --show-error "$BASE_URL/$NODE_ARCHIVE" --output "$WORK/$NODE_ARCHIVE"
EXPECTED="$(awk -v file="$NODE_ARCHIVE" '$2 == file { print $1 }' "$WORK/SHASUMS256.txt")"
[[ -n "$EXPECTED" ]] || { echo "Node checksum not found for $NODE_ARCHIVE" >&2; exit 1; }
[[ "$(sha256 "$WORK/$NODE_ARCHIVE")" == "$EXPECTED" ]] || { echo "Node checksum mismatch" >&2; exit 1; }

mkdir -p "$WORK/node" "$WORK/runtime"
if [[ "$ARCHIVE_EXT" == "zip" ]]; then
    unzip -q "$WORK/$NODE_ARCHIVE" -d "$WORK/node"
    NODE_ROOT="$WORK/node/node-v$NODE_VERSION-$NODE_TARGET"
else
    tar -xf "$WORK/$NODE_ARCHIVE" -C "$WORK/node" --strip-components=1
    NODE_ROOT="$WORK/node"
fi

cp "$NODE_ROOT/$NODE_SOURCE" "$WORK/runtime/$NODE_FILE"
cp "$NODE_ROOT/LICENSE" "$WORK/runtime/NODE-LICENSE"
cp "$ROOT/scripts/patchright-sidecar.cjs" "$WORK/runtime/sidecar.cjs"
npm install \
    --prefix "$WORK/runtime" \
    --omit=dev \
    --ignore-scripts \
    --no-audit \
    --no-fund \
    --no-package-lock \
    "patchright-core@$PATCHRIGHT_VERSION" \
    "@duckduckgo/autoconsent@$AUTOCONSENT_VERSION"

curl --fail --location --silent --show-error \
    "https://raw.githubusercontent.com/Kaliiiiiiiiii-Vinyzu/patchright/v$PATCHRIGHT_VERSION/LICENSE" \
    --output "$WORK/runtime/PATCHRIGHT-LICENSE"
cp "$WORK/runtime/node_modules/@duckduckgo/autoconsent/LICENSE" "$WORK/runtime/AUTOCONSENT-LICENSE"
printf '{"node":"%s","patchright":"%s","autoconsent":"%s","target":"%s"}\n' \
    "$NODE_VERSION" "$PATCHRIGHT_VERSION" "$AUTOCONSENT_VERSION" "$TARGET" > "$WORK/runtime/runtime.json"

mkdir -p "$(dirname "$OUTPUT")"
"$PYTHON" "$ROOT/scripts/archive-runtime.py" "$WORK/runtime" "$WORK/runtime.tar.gz"
sha256 "$WORK/runtime.tar.gz" > "$WORK/runtime.tar.gz.sha256"
mv "$WORK/runtime.tar.gz" "$OUTPUT"
mv "$WORK/runtime.tar.gz.sha256" "$OUTPUT.sha256"
echo "prepared $OUTPUT"
