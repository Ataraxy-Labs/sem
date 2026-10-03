#!/usr/bin/env sh
# Install the pinned Node tools (typescript, eslint, vitest) that the `sem check`
# integration tests run their fixture projects with, into crates/target/check-tools.
# The tests find them there (or at $SEM_CHECK_TOOLS); without them the Node-backed
# tests are skipped, unless SEM_CHECK_REQUIRE_TOOLS=1 makes their absence a failure.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
dest="$here/crates/target/check-tools"
mkdir -p "$dest"
cp "$here/crates/sem-cli/tests/fixtures/check-tools/package.json" "$dest/package.json"
cd "$dest"
npm install --no-audit --no-fund --silent
echo "installed into $dest/node_modules"
