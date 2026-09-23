#!/usr/bin/env bash
# Upload a build artifact to the owner-scoped Forgejo registry and link its
# package to this source repository, so it appears on /toxblh/matras/packages.
# Usage: publish-package.sh generic NAME VERSION FILE...
#        publish-package.sh alt RPM_FILE...
set -Eeuo pipefail

: "${PACKAGES_TOKEN:?Set the PACKAGES_TOKEN Actions secret with write:package scope}"
owner=${PACKAGES_OWNER:-toxblh}
repo=${PACKAGES_REPO:-matras}
server=${FORGEJO_SERVER_URL:-https://altlinux.space}
auth=(--user "$owner:$PACKAGES_TOKEN")

link_package() {
    local type=$1 name=$2 version=$3 status linked
    status=$(curl --silent --show-error --output /dev/null --write-out '%{http_code}' \
        "${auth[@]}" --request POST \
        "$server/api/v1/packages/$owner/$type/$name/-/link/$repo")
    case $status in
        2??) return 0 ;;
        # Forgejo returns 400 if another architecture already linked this
        # package. Verify the association before treating that as success.
        400|409)
            linked=$(curl --fail --silent --show-error "${auth[@]}" \
                "$server/api/v1/packages/$owner/$type/$name/$version" | \
                python3 -c 'import json,sys; print((json.load(sys.stdin).get("repository") or {}).get("full_name", ""))')
            [[ $linked == "$owner/$repo" ]] && return 0
            ;;
    esac
    echo "Could not link $type/$name to $repo (HTTP $status)" >&2
    return 1
}

case ${1:-} in
    generic)
        (( $# >= 4 )) || { echo 'usage: publish-package.sh generic NAME VERSION FILE...' >&2; exit 2; }
        name=$2 version=$3
        shift 3
        for file in "$@"; do
            test -s "$file"
            filename=${file##*/}
            curl --fail --silent --show-error --retry 3 \
                "${auth[@]}" --upload-file "$file" \
                "$server/api/packages/$owner/generic/$name/$version/$filename" >/dev/null
            echo "Published $name/$version/$filename"
        done
        link_package generic "$name" "$version"
        ;;
    alt)
        (( $# >= 2 )) || { echo 'usage: publish-package.sh alt RPM_FILE...' >&2; exit 2; }
        shift
        for file in "$@"; do
            test -s "$file"
            name=$(rpm -qp --qf '%{NAME}' "$file")
            group_path=
            if [[ -n ${PACKAGES_ALT_GROUP:-} ]]; then
                group_path=/group/$PACKAGES_ALT_GROUP
            fi
            curl --fail --silent --show-error --retry 3 \
                "${auth[@]}" --upload-file "$file" \
                "$server/api/packages/$owner/alt$group_path/upload" >/dev/null
            echo "Published ALT RPM ${file##*/}"
            version=$(rpm -qp --qf '%{VERSION}-%{RELEASE}' "$file")
            link_package alt "$name" "$version"
        done
        ;;
    *) echo 'usage: publish-package.sh {generic NAME VERSION FILE...|alt RPM_FILE...}' >&2; exit 2 ;;
esac
