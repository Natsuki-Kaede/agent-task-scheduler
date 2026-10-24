#!/bin/sh
# Builds the release archive for Apple Silicon into dist/ and fills in the
# Homebrew formula with its version and checksum.
set -eu
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
target=aarch64-apple-darwin
name="ats-$version-$target"

cargo build --release --locked --target "$target"
rm -rf "dist/$name"
mkdir -p "dist/$name"
cp "target/$target/release/ats" README.md LICENSE "dist/$name/"
tar -C dist -czf "dist/$name.tar.gz" "$name"
sha=$(shasum -a 256 "dist/$name.tar.gz" | cut -d ' ' -f 1)
sed -e "s/@VERSION@/$version/g" -e "s/@SHA256@/$sha/g" packaging/homebrew/ats.rb > dist/ats.rb

echo "dist/$name.tar.gz"
echo "sha256 $sha"
echo "dist/ats.rb"
