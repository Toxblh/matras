#!/bin/sh
# Fetch the two Pixiewood inputs at the revisions used by the working local APK.
set -eu

tools_dir=${1:-.android-tools}
mkdir -p "$tools_dir"

fetch_tool() {
    name=$1
    url=$2
    branch=$3
    revision=$4
    destination="$tools_dir/$name"

    if [ ! -d "$destination/.git" ]; then
        git clone --quiet --filter=blob:none --branch "$branch" "$url" "$destination"
    fi
    if [ "$(git -C "$destination" rev-parse HEAD)" != "$revision" ]; then
        git -C "$destination" fetch --quiet origin "$revision"
        git -C "$destination" checkout --quiet --detach "$revision"
    fi
    test "$(git -C "$destination" rev-parse HEAD)" = "$revision"
}

fetch_tool pixiewood https://github.com/sp1ritCS/gtk-android-builder.git android36 \
    de3719725a8f6e3efe0f28203e803e7ce1904902
fetch_tool mini-studio https://github.com/sp1ritCS/mini-studio.git master \
    8c2901968b07245ad07c1b38270928a49a510765
