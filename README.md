# nx-aarch64

This is a minimal LLVM toolchain for compiling and linking for ARM64.

This is sourced directly from llvm-project and not related to Nintendo's proprietary toolchain.

## Install
Download and extract a release for the current platform (linux-x64, linux-arm64 or macos-arm64)
to `$OUTPUT/nx-aarch64-$VERSION/`. The guard skips the download if that version is already installed.

```sh
VERSION=22.1.8-1
OUTPUT=path/to/dir
[ -d "$OUTPUT/nx-aarch64-$VERSION" ] || curl -fsSL https://raw.githubusercontent.com/Pistonight/nx-aarch64/main/install.sh | VERSION="$VERSION" OUTPUT="$OUTPUT" sh
```
