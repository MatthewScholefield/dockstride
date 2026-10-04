#!/bin/sh
# Build only with explicit, existing host tools or an explicitly selected container.
set -eu

usage() {
    cat <<'USAGE'
Usage: scripts/release.sh [--target TARGET] [--output DIR] [--test]
                          [--verify-reproducible] [--container [IMAGE]]

Native Linux x86_64/ARM64 only. Host mode needs cargo, the installed musl Rust
 target, musl-gcc, ar, readelf, GNU tar, gzip, and sha256sum. Nothing is installed.
Container mode explicitly uses Docker and a Rust Alpine compiler image (default:
 rust:alpine), resolving its immutable digest before compilation. Docker remains
 an external prerequisite. Compilation never installs packages on the host.
Set SOURCE_DATE_EPOCH for a chosen archive timestamp; otherwise use the checkout
 commit timestamp (or zero outside a Git checkout). Cargo.lock is mandatory.
USAGE
}
fail() { printf 'release: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || fail "missing prerequisite: $1"; }
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output=$root/dist
container=0
image=rust:alpine
run_tests=0
repro=0
target=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --help|-h) usage; exit 0 ;;
        --target|--output)
            [ "$#" -ge 2 ] || fail "$1 requires a value"
            if [ "$1" = --target ]; then target=$2; else output=$2; fi
            shift 2 ;;
        --container)
            container=1; shift
            if [ "$#" -gt 0 ]; then
                case "$1" in --*) ;; *) image=$1; shift ;; esac
            fi ;;
        --test) run_tests=1; shift ;;
        --verify-reproducible) repro=1; shift ;;
        *) fail "unknown argument: $1" ;;
    esac
done
[ "$(uname -s)" = Linux ] || fail 'Linux is required'
case "$(uname -m)" in
    x86_64) native=x86_64-unknown-linux-musl; platform=linux/amd64; machine='Advanced Micro Devices X86-64' ;;
    aarch64|arm64) native=aarch64-unknown-linux-musl; platform=linux/arm64; machine=AArch64 ;;
    *) fail 'only native Linux x86_64 and ARM64 are supported' ;;
esac
[ -n "$target" ] || target=$native
[ "$target" = "$native" ] || fail "target $target is not native to this machine; use its native runner"
[ -f "$root/Cargo.lock" ] || fail 'Cargo.lock is required for --locked builds'
for tool in readelf tar gzip sha256sum cmp mktemp; do need "$tool"; done
case "$(tar --version)" in *'GNU tar'*) ;; *) fail 'GNU tar is required for deterministic archives' ;; esac
case "$root$output" in *'
'*|*' '*) fail 'build and output paths must not contain spaces or newlines' ;; esac
mkdir -p "$output"
output=$(CDPATH= cd -- "$output" && pwd)
source_revision=unversioned
if command -v git >/dev/null 2>&1 && [ "$(git -C "$root" rev-parse --show-toplevel 2>/dev/null || :)" = "$root" ] && git -C "$root" rev-parse --verify HEAD >/dev/null 2>&1; then
    source_revision=$(git -C "$root" rev-parse HEAD)
    epoch=${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --format=%ct)}
else
    epoch=${SOURCE_DATE_EPOCH:-0}
fi
case "$epoch" in ''|*[!0-9]*) fail 'SOURCE_DATE_EPOCH must be a nonnegative integer' ;; esac
export SOURCE_DATE_EPOCH=$epoch
# Read the package version without introducing a JSON/Python build prerequisite.
version=
in_package=0
while IFS= read -r line; do
    case "$line" in
        '[package]') in_package=1 ;;
        '['*) [ "$in_package" -eq 0 ] || break ;;
        'version = "'*)
            if [ "$in_package" -eq 1 ]; then version=${line#version = \"}; version=${version%\"}; fi ;;
    esac
done < "$root/Cargo.toml"
case "$version" in ''|*[!0-9A-Za-z.+-]*) fail 'cannot read Cargo.toml package version' ;; esac
key=$(printf '%s' "$target" | tr '-' '_')
upper=$(printf '%s' "$key" | tr '[:lower:]' '[:upper:]')
if [ "$container" -eq 1 ]; then
    need docker
    # Rootless namespace UID zero is the invoking host user, not host root.
    # A literal host UID inside that namespace maps to a subordinate host UID.
    case "$(docker info --format '{{json .SecurityOptions}}')" in
        *rootless*) container_user=0:0 ;;
        *) container_user="$(id -u):$(id -g)" ;;
    esac
    printf 'Resolving explicit compiler container %s\n' "$image" >&2
    docker pull --platform "$platform" "$image" >&2
    pinned_image=$(docker image inspect --format '{{index .RepoDigests 0}}' "$image")
    case "$pinned_image" in *@sha256:*) ;; *) fail 'compiler image has no immutable registry digest' ;; esac
else
    need cargo; need rustc; need ar
    compiler=${MUSL_CC:-musl-gcc}
    command -v "$compiler" >/dev/null 2>&1 || fail "missing $compiler; install musl-tools explicitly, or run scripts/release.sh --container"
    pinned_image=none
fi
build() {
    build_dir=$1
    tests=$2
    mkdir -p "$build_dir"
    if [ "$container" -eq 1 ]; then
        relative_dir=${build_dir#"$root"/}
        docker run --rm --platform "$platform" --user "$container_user" \
            --volume "$root:/work:Z" --workdir /work \
            --env CARGO_HOME=/tmp/dockstride-cargo --env CARGO_TARGET_DIR="/work/$relative_dir" \
            --env SOURCE_DATE_EPOCH --env BUILD_TARGET="$target" --env TARGET_KEY="$key" \
            --env TARGET_UPPER="$upper" --env RUN_TESTS="$tests" \
            --env "RUSTFLAGS=--remap-path-prefix=/work=/dockstride --remap-path-prefix=/work/$relative_dir=/dockstride/target -C target-feature=+crt-static" \
            --env "CFLAGS=-ffile-prefix-map=/work=/dockstride -ffile-prefix-map=/work/$relative_dir=/dockstride/target" \
            --entrypoint sh "$pinned_image" -ec '
                command -v cargo >/dev/null
                command -v gcc >/dev/null
                command -v ar >/dev/null
                rustc -vV > "$CARGO_TARGET_DIR/compiler-info.txt"
                cargo --version >> "$CARGO_TARGET_DIR/compiler-info.txt"
                gcc --version >> "$CARGO_TARGET_DIR/compiler-info.txt"
                export CC=gcc AR=ar
                if [ "$RUN_TESTS" = 1 ]; then
                    env "CC_$TARGET_KEY=gcc" "AR_$TARGET_KEY=ar" "CARGO_TARGET_${TARGET_UPPER}_LINKER=gcc" cargo test --locked --target "$BUILD_TARGET" --all-targets
                fi
                env "CC_$TARGET_KEY=gcc" "AR_$TARGET_KEY=ar" "CARGO_TARGET_${TARGET_UPPER}_LINKER=gcc" cargo build --locked --release --target "$BUILD_TARGET" --bin dks
            '
    else
        rustc -vV > "$build_dir/compiler-info.txt"
        cargo --version >> "$build_dir/compiler-info.txt"
        "$compiler" --version >> "$build_dir/compiler-info.txt"
        flags="--remap-path-prefix=$root=/dockstride --remap-path-prefix=$build_dir=/dockstride/target -C target-feature=+crt-static"
        cflags="-ffile-prefix-map=$root=/dockstride -ffile-prefix-map=$build_dir=/dockstride/target"
        if [ "$tests" -eq 1 ]; then
            (cd "$root" && env "CC_$key=$compiler" "AR_$key=ar" "CARGO_TARGET_${upper}_LINKER=$compiler" "CARGO_TARGET_DIR=$build_dir" "RUSTFLAGS=$flags" "CFLAGS=$cflags" cargo test --locked --target "$target" --all-targets)
        fi
        (cd "$root" && env "CC_$key=$compiler" "AR_$key=ar" "CARGO_TARGET_${upper}_LINKER=$compiler" "CARGO_TARGET_DIR=$build_dir" "RUSTFLAGS=$flags" "CFLAGS=$cflags" cargo build --locked --release --target "$target" --bin dks)
    fi
}
first=$root/target/release-build
build "$first" "$run_tests"
binary=$first/$target/release/dks
[ -x "$binary" ] || fail "build did not produce executable $binary"
header=$(readelf -h "$binary")
case "$header" in *"$machine"*) ;; *) fail 'ELF machine does not match the native target' ;; esac
program_headers=$(readelf -l "$binary")
case "$program_headers" in *INTERP*) fail 'binary has a dynamic ELF interpreter' ;; esac
dynamic=$(readelf -d "$binary")
case "$dynamic" in *NEEDED*) fail 'binary depends on shared libraries' ;; esac
"$binary" --version
if [ "$repro" -eq 1 ]; then
    # Fresh directories prove byte-for-byte identity, not just a Cargo cache hit.
    second=$(mktemp -d "$root/target/repro-build.XXXXXX")
    build "$second" 0
    cmp "$binary" "$second/$target/release/dks" || fail 'independent release builds are not byte-identical'
    printf 'Independent static binary builds are byte-identical.\n' >&2
    rm -rf "$second"
fi
stage=$(mktemp -d "$output/.release.XXXXXX")
trap 'rm -rf "$stage"' EXIT HUP INT TERM
name=dockstride-$version-$target
package=$stage/$name
mkdir -p "$package/sample/libs"
cp "$binary" "$package/dks"
cp "$root/LICENSE" "$package/LICENSE"
cp "$root/Cargo.lock" "$package/Cargo.lock"
# Match dks init: checked-in library, hello-world configuration, and local files.
cp "$root/assets/dockstride.ncl" "$package/sample/libs/dockstride.ncl"
cp "$root/assets/compose.ncl" "$package/sample/compose.ncl"
printf '{}\n' > "$package/sample/env.yaml"
printf '/env.yaml\n/.dockstride/\n' > "$package/sample/.gitignore"
{
    printf 'package=%s\nversion=%s\ntarget=%s\nsource_revision=%s\nsource_date_epoch=%s\ncompiler_image=%s\n' dockstride "$version" "$target" "$source_revision" "$epoch" "$pinned_image"
    printf 'cargo_lock_sha256=%s\n' "$(sha256sum "$root/Cargo.lock" | cut -d ' ' -f 1)"
    cat "$first/compiler-info.txt"
} > "$package/BUILD-INFO.txt"
(cd "$package" && sha256sum dks Cargo.lock LICENSE BUILD-INFO.txt sample/libs/dockstride.ncl sample/compose.ncl sample/env.yaml sample/.gitignore > SHA256SUMS)
chmod 0755 "$package" "$package/sample" "$package/sample/libs" "$package/dks"
chmod 0644 "$package/LICENSE" "$package/Cargo.lock" "$package/BUILD-INFO.txt" "$package/SHA256SUMS" \
    "$package/sample/libs/dockstride.ncl" "$package/sample/compose.ncl" \
    "$package/sample/env.yaml" "$package/sample/.gitignore"
archive=$name.tar.gz
tar --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner --format=gnu -C "$stage" -cf "$stage/$name.tar" "$name"
gzip -n -9 "$stage/$name.tar"
mv "$stage/$archive" "$output/$archive"
(cd "$output" && sha256sum "$archive" > "$archive.sha256")
printf 'Static native release: %s\nChecksum: %s\n' "$output/$archive" "$output/$archive.sha256"
