#!/usr/bin/env bash
# Publish a Flatpak OCI image to Forgejo's container registry.
# Usage: publish-oci-flatpak.sh ARCH FILE.oci [TAG...]
set -Eeuo pipefail

(( $# >= 2 )) || { echo "usage: ${0##*/} ARCH FILE.oci [TAG...]" >&2; exit 2; }
arch=$1 image=$2
shift 2
tags=("$@")
((${#tags[@]})) || tags=(stable oci-test)

if [[ -z ${PACKAGES_TOKEN:-} ]]; then
  echo 'Skipping OCI publish: add the PACKAGES_TOKEN secret'
  exit 0
fi
test -s "$image"
owner=${PACKAGES_OWNER:-toxblh}
server=${FORGEJO_SERVER_URL:-https://altlinux.space}
registry=${server#https://}
registry=${registry#http://}
repository="${owner}/ru.toxblh.matras-${arch}-stable"

for tag in "${tags[@]}"; do
  skopeo copy --retry-times 3 \
    --dest-creds "$owner:$PACKAGES_TOKEN" \
    "oci-archive:$image" "docker://$registry/$repository:$tag"
  echo "Published Flatpak OCI ${repository}:$tag"
done
