#!/usr/bin/env bash
# Smoke-test hdc binaries: start server mode, then run list targets.
set -u
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

bin="${1:-target/dist/hdc-linux-x86_64/hdc}"
echo "== testing $bin"
"$bin" -m >/tmp/hdc_server.log 2>&1 &
server_pid=$!
sleep 2
"$bin" list targets -v 2>&1 | head -3
rc=$?
kill "$server_pid" 2>/dev/null
wait "$server_pid" 2>/dev/null
echo "== list targets exit=$rc"
