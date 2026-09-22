#!/bin/sh
# Build from a clean checkout with the same Linux toolchain locally and in Forgejo.
set -eu

cd "$(dirname "$0")/../.."
image=matras-android-build:local
container="matras-android-$$"
file_list=$(mktemp)
source_tar=$(mktemp)

cleanup() {
    docker rm -f "$container" >/dev/null 2>&1 || true
    rm -f "$file_list" "$source_tar"
}
trap cleanup EXIT HUP INT TERM

# Include tracked and new source files while excluding generated build trees.
git ls-files --cached --others --exclude-standard -z > "$file_list"
tar --null -T "$file_list" -cf "$source_tar"

docker build --platform linux/amd64 -t "$image" -f build-aux/android/Dockerfile build-aux/android
docker create --platform linux/amd64 --name "$container" \
    --mount source=matras-cargo-registry,target=/opt/cargo/registry \
    --mount source=matras-gradle,target=/root/.gradle \
    -w /work/matras "$image" sh -ec '
        make -f build-aux/android/android.mk android release=1
        mkdir -p /work/artifacts
        cp .pixiewood/android/app/build/outputs/apk/release/app-arm64-v8a-release-unsigned.apk \
           /work/artifacts/Matras-arm64-v8a-release-unsigned.apk
    ' >/dev/null
docker cp - "$container:/work/matras" < "$source_tar"
docker start -a "$container"
mkdir -p apks
docker cp "$container:/work/artifacts/." apks/
test -s apks/Matras-arm64-v8a-release-unsigned.apk
