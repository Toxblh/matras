#!/bin/sh
# Sign an already aligned APK using the same secrets locally or in Forgejo.
set -eu
: "${ANDROID_HOME:?Android SDK path is required}"
: "${ANDROID_KEYSTORE_B64:?base64 keystore is required}"
: "${ANDROID_KEYSTORE_PASSWORD:?keystore password is required}"
: "${ANDROID_KEY_ALIAS:?key alias is required}"

unsigned_apk=$1
signed_apk=$2
umask 077
signing_dir=$(mktemp -d)
trap 'rm -rf "$signing_dir"' 0
printf '%s' "$ANDROID_KEYSTORE_B64" | base64 -d > "$signing_dir/keystore.p12"
"$ANDROID_HOME/build-tools/36.0.0/apksigner" sign \
    --ks "$signing_dir/keystore.p12" \
    --ks-key-alias "$ANDROID_KEY_ALIAS" \
    --ks-pass env:ANDROID_KEYSTORE_PASSWORD \
    --in "$unsigned_apk" --out "$signed_apk"
"$ANDROID_HOME/build-tools/36.0.0/apksigner" verify --verbose "$signed_apk"
