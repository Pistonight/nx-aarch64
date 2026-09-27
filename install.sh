#!/bin/sh
# Download and extract an nx-aarch64 release for the current platform.
#
# Usage: VERSION=22.1.8-1 OUTPUT=path/to/dir ./install.sh
#
# Or without cloning the repo:
#   curl -fsSL https://raw.githubusercontent.com/Pistonight/nx-aarch64/main/install.sh | VERSION=22.1.8-1 OUTPUT=path/to/dir sh
#
# The toolchain is extracted to $OUTPUT/nx-aarch64-$VERSION/ (bin/ and include/),
# replacing it if it already exists.
set -eu

REPO="Pistonight/nx-aarch64"

if [ -z "${VERSION:-}" ]; then
    echo "error: VERSION is not set (e.g. VERSION=22.1.8-1)" >&2
    exit 1
fi
if [ -z "${OUTPUT:-}" ]; then
    echo "error: OUTPUT is not set" >&2
    exit 1
fi

case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    *) echo "error: unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
    x86_64|amd64) arch=x64 ;;
    aarch64|arm64) arch=arm64 ;;
    *) echo "error: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
target="$os-$arch"
if [ "$target" = "macos-x64" ]; then
    echo "error: macos-x64 is not supported" >&2
    exit 1
fi

name="nx-aarch64-$VERSION"
package="$name-$target.tar.xz"
url="https://github.com/$REPO/releases/download/$VERSION/$package"
dest="$OUTPUT/$name"

mkdir -p "$OUTPUT"
tmp="$(mktemp -d "$OUTPUT/.$name.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

echo "downloading $url"
curl -fL --retry 3 -o "$tmp/$package" "$url"

echo "extracting to $dest"
mkdir "$tmp/$name"
tar -xJf "$tmp/$package" -C "$tmp/$name"
rm -rf "$dest"
mv "$tmp/$name" "$dest"

echo "installed $dest"
