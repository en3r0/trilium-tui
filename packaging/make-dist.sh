#!/bin/bash
# Packs an already-built target/release into dist/trilium-tui-<version>-<arch>-linux.tar.gz (+ .sha256).
# usage: packaging/make-dist.sh <version> <arch>      e.g. packaging/make-dist.sh 0.1.0 x86_64
set -euo pipefail
cd "$(dirname "$0")/.."
ver=${1:?version}; arch=${2:?arch}
name=trilium-tui-$ver-$arch-linux
out=dist/$name
rm -rf "$out" "dist/$name.tar.gz" "dist/$name.tar.gz.sha256"
mkdir -p "$out/completions"
install -m755 target/release/trilium-tui target/release/trilium-cli "$out/"
strip "$out/trilium-tui" "$out/trilium-cli"
"$out/trilium-cli" completions bash > "$out/completions/trilium-cli.bash"
"$out/trilium-cli" completions zsh  > "$out/completions/_trilium-cli"
"$out/trilium-cli" completions fish > "$out/completions/trilium-cli.fish"
cp LICENSE README.md docs/FEATURES.md packaging/trilium-tui.desktop "$out/"
install -m755 packaging/install-binary.sh "$out/install.sh"
tar -C dist -czf "dist/$name.tar.gz" "$name"
(cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "dist/$name.tar.gz"
