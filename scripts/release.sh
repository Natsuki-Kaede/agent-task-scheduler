#!/bin/sh
# Builds the release archive for one target into dist/. Without an
# argument, the target is the host's.
set -eu
cd "$(dirname "$0")/.."

target=${1:-$(rustc -vV | sed -n 's/^host: //p')}
name="ats-$target"

cargo build --release --locked --target "$target"
rm -rf "dist/$name"
mkdir -p "dist/$name"
cp "target/$target/release/ats" README.md LICENSE "dist/$name/"
tar -C dist -czf "dist/$name.tar.gz" "$name"
echo "dist/$name.tar.gz"
