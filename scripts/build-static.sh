#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output=$root/target/server-static
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output)
            [ "$#" -ge 2 ] || { printf '%s\n' '--output requires a directory' >&2; exit 2; }
            output=$2
            shift 2
            ;;
        --help|-h)
            printf '%s\n' 'Usage: sh scripts/build-static.sh [--output DIR]' \
                'Build a native static Linux dks with persistent BuildKit Cargo caches.' \
                'Output defaults to target/server-static/dks; DKS_STATIC_BUILDER selects the persistent builder.' \
                'Automatic cache pruning is disabled; reclaim disk manually with docker buildx prune --builder dockstride-static.'
            exit 0
            ;;
        *) printf 'Unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done
[ "$(uname -s)" = Linux ] || { printf '%s\n' 'Linux is required.' >&2; exit 1; }
case "$(uname -m)" in
    x86_64) platform=linux/amd64 ;;
    aarch64|arm64) platform=linux/arm64 ;;
    *) printf '%s\n' 'Only x86_64 and ARM64 are supported.' >&2; exit 1 ;;
esac
command -v docker >/dev/null
command -v readelf >/dev/null
builder=${DKS_STATIC_BUILDER:-dockstride-static}
if ! docker buildx inspect "$builder" >/dev/null 2>&1; then
    docker buildx create --name "$builder" --driver docker-container \
        --buildkitd-flags '--oci-worker-gc=false' >/dev/null
fi
docker buildx build --builder "$builder" --platform "$platform" --file "$root/Dockerfile" \
    --output "type=local,dest=$output" "$root"
binary=$output/dks
[ -x "$binary" ]
if readelf -l "$binary" | grep -q INTERP || readelf -d "$binary" | grep -q NEEDED; then
    printf '%s\n' 'Build produced a dynamically linked executable.' >&2
    exit 1
fi
"$binary" --version
sha256sum "$binary"
