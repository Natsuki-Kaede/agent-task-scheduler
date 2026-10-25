#!/bin/sh
# Writes dist/SHA256SUMS and the Homebrew formula dist/ats.rb from the
# release archives in dist/. The formula needs the archives of all four
# targets.
set -eu
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
script="s/@VERSION@/$version/g"
: > dist/SHA256SUMS
for archive in dist/ats-*.tar.gz; do
    file=${archive#dist/}
    target=${file#ats-}
    target=${target%.tar.gz}
    sha=$(shasum -a 256 "$archive" | cut -d ' ' -f 1)
    echo "$sha  $file" >> dist/SHA256SUMS
    script="$script;s/@SHA256_$target@/$sha/g"
done
sed -e "$script" packaging/homebrew/ats.rb > dist/ats.rb
if grep -q '@SHA256_' dist/ats.rb; then
    echo "missing archives: $(grep -o '@SHA256_[^@]*@' dist/ats.rb | tr '\n' ' ')" >&2
    exit 1
fi
echo dist/SHA256SUMS
echo dist/ats.rb
