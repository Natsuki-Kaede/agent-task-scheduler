#!/bin/sh
# Runs the project's required checks.
set -eu
cd "$(dirname "$0")/.."

cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test

if [ -f web/package.json ]; then
	(cd web && npm test && npm run build)
fi
