#!/usr/bin/env bash
# Build hdc release binaries for Linux (x86_64 / aarch64, glibc / musl).
#
# Usage: scripts/build-release.sh [target ...]
#   Available targets:
#     x86_64-gnu           x86_64-unknown-linux-gnu      (host gcc, host glibc)
#     x86_64-glibc2.24     x86_64 + glibc >= 2.24        (zig cc, glibc 2.24 baseline)
#     x86_64-musl          x86_64 fully static           (musl-gcc)
#     aarch64-gnu          aarch64-unknown-linux-gnu     (aarch64-linux-gnu-gcc)
#     aarch64-glibc2.24    aarch64 + glibc >= 2.24       (zig cc, glibc 2.24 baseline)
#     aarch64-musl         aarch64 fully static          (zig cc)
#
# Artifacts are placed in target/dist/<name>/.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo"

# 发布构建必须使用 vendored C 库，不接受机器上的系统 pkg-config 结果。
export PKG_CONFIG_LIBDIR="/__hdc_rust_no_pkg_config__"
export PKG_CONFIG_PATH="/__hdc_rust_no_pkg_config__"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo/target/standalone}"

dist="$repo/target/dist"
mkdir -p "$dist"

zig="${ZIG:-$HOME/zig/zig}"
wrapper_dir="$repo/target/wrappers"
mkdir -p "$wrapper_dir"

make_zig_wrapper() { # name zig-target
    local w="$wrapper_dir/$1"
    cat > "$w" <<EOF
#!/usr/bin/env bash
# cc-rs 会给编译器传 rust 风格 --target=<triple>（如 x86_64-unknown-linux-gnu），
# zig 0.16 无法解析其中的 "unknown" OS 段，这里过滤掉，改用 zig 的 -target。
args=()
for a in "\$@"; do
    case "\$a" in
        --target=*) ;;
        *) args+=("\$a") ;;
    esac
done
exec "$zig" cc -target "$2" "\${args[@]}"
EOF
    chmod +x "$w"
}

build_target() { # rust-target cc-env-name artifact-name [zig-target-or-empty] [linker-or-empty]
    local rust_target="$1"
    local cc_env="$2"
    local artifact="$3"
    local zig_target="${4:-}"
    local linker="${5:-}"
    local out="$dist/$artifact"
    mkdir -p "$out"

    local extra_env=()
    if [[ -n "$zig_target" ]]; then
        make_zig_wrapper "cc-${artifact}" "$zig_target"
        make_zig_wrapper "ld-${artifact}" "$zig_target"
        extra_env=("RUSTFLAGS=-C linker=$wrapper_dir/ld-$artifact")
    elif [[ -n "$linker" ]]; then
        extra_env=("RUSTFLAGS=-C linker=$linker")
    fi

    echo "==> building $rust_target -> $artifact"
    env "CC_${rust_target//-/_}=$cc_env" "${extra_env[@]+"${extra_env[@]}"}" \
        cargo build --release --target "$rust_target" --bin hdc
    cp "$CARGO_TARGET_DIR/$rust_target/release/hdc" "$out/hdc"
    echo "    artifact: $out/hdc"
}

if [[ $# -eq 0 ]]; then
    set -- x86_64-gnu aarch64-gnu x86_64-musl aarch64-musl x86_64-glibc2.24 aarch64-glibc2.24
fi

for t in "$@"; do
    case "$t" in
        x86_64-gnu)
            build_target x86_64-unknown-linux-gnu gcc hdc-linux-x86_64
            ;;
        aarch64-gnu)
            build_target aarch64-unknown-linux-gnu aarch64-linux-gnu-gcc \
                hdc-linux-aarch64 "" aarch64-linux-gnu-gcc
            ;;
        x86_64-musl)
            # Debian 的 musl 头隔离了 Linux UAPI 头：musl 头优先，
            # 再从 /usr/include（linux/…）与 multiarch 目录（asm/…）补充。
            CFLAGS_x86_64_unknown_linux_musl="-idirafter /usr/include -idirafter /usr/include/x86_64-linux-gnu" \
            build_target x86_64-unknown-linux-musl musl-gcc hdc-linux-x86_64-musl-static
            ;;
        aarch64-musl)
            # zig cc 与 rustc 会各自注入 musl crt1.o 导致 _start 重复定义，
            # 因此 aarch64 musl 使用 musl.cc 交叉工具链（自带 musl + Linux UAPI 头）。
            # 下载: https://musl.cc/aarch64-linux-musl-cross.tgz
            muslcc="${MUSL_CROSS_DIR:-$HOME/opt/aarch64-linux-musl-cross/bin}/aarch64-linux-musl-gcc"
            build_target aarch64-unknown-linux-musl "$muslcc" \
                hdc-linux-aarch64-musl-static "" "$muslcc"
            ;;
        x86_64-glibc2.24)
            build_target x86_64-unknown-linux-gnu "$wrapper_dir/cc-hdc-linux-x86_64-glibc2.24" \
                hdc-linux-x86_64-glibc2.24 x86_64-linux-gnu.2.24
            ;;
        aarch64-glibc2.24)
            build_target aarch64-unknown-linux-gnu "$wrapper_dir/cc-hdc-linux-aarch64-glibc2.24" \
                hdc-linux-aarch64-glibc2.24 aarch64-linux-gnu.2.24
            ;;
        *)
            echo "unknown target: $t" >&2
            exit 2
            ;;
    esac
done

echo "done."
