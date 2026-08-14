#!/usr/bin/env bash
# Verify release artifacts in target/dist: architecture, glibc version
# requirements and static linkage of musl builds.
set -u

dist="${1:-target/dist}"
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

echo "===== ARCHITECTURE ====="
for f in "$dist"/*/hdc; do
    printf '%-50s %s\n' "$f" "$(file -b "$f")"
done

echo "===== GLIBC VERSION REQUIREMENTS (dynamic builds) ====="
for f in "$dist"/hdc-linux-x86_64/hdc "$dist"/hdc-linux-x86_64-glibc2.24/hdc \
         "$dist"/hdc-linux-aarch64/hdc "$dist"/hdc-linux-aarch64-glibc2.24/hdc; do
    if [[ -f "$f" ]]; then
        maxver=$(readelf --version-info "$f" | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1)
        printf '%-50s max %s\n' "$f" "$maxver"
    fi
done

echo "===== NEEDED COUNT (musl static must be 0) ====="
for f in "$dist"/hdc-linux-x86_64-musl-static/hdc "$dist"/hdc-linux-aarch64-musl-static/hdc; do
    if [[ -f "$f" ]]; then
        n=$(readelf -d "$f" | grep -c NEEDED || true)
        printf '%-50s NEEDED=%s\n' "$f" "$n"
    fi
done
