#!/bin/sh
# Run inside the Forgejo Linux/amd64 job container. No Docker socket required.
set -eu

export DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8
export ANDROID_HOME=/opt/android-sdk ANDROID_SDK_ROOT=/opt/android-sdk
export ANDROID_NDK_HOME=/opt/android-sdk/ndk/27.2.12479018
export ANDROID_NDK_ROOT=$ANDROID_NDK_HOME
export RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
ndk_bin=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin
export NDK_BIN=$ndk_bin
export PATH=/opt/cargo/bin:$ndk_bin:$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$PATH

apt-get update
apt-get install -y --no-install-recommends \
    libglib-perl libglib-object-introspection-perl libipc-run-perl \
    libjson-perl libset-scalar-perl libxml-libxml-perl libxml-libxslt-perl \
    gir1.2-appstream-1.0 openjdk-17-jdk-headless \
    build-essential glslc gobject-introspection libglib2.0-dev-bin \
    libxml2-utils ninja-build sassc gettext pkg-config cmake \
    git curl wget unzip ca-certificates python3 python3-pip xz-utils
python3 -m pip install --break-system-packages --no-cache-dir meson==1.12.0

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable
rustup target add aarch64-linux-android

mkdir -p "$ANDROID_HOME/cmdline-tools"
curl -fsSL -o /tmp/matras-cmdline.zip \
    https://dl.google.com/android/repository/commandlinetools-linux-13114758_latest.zip
unzip -q /tmp/matras-cmdline.zip -d /tmp
mv /tmp/cmdline-tools "$ANDROID_HOME/cmdline-tools/latest"
rm /tmp/matras-cmdline.zip
yes | sdkmanager --licenses > /dev/null 2>&1 || true
sdkmanager --install 'platform-tools' 'platforms;android-36' \
    'build-tools;36.0.0' 'ndk;27.2.12479018'

cat > "$CARGO_HOME/config.toml" <<EOF
[target.aarch64-linux-android]
linker = "$ndk_bin/aarch64-linux-android31-clang"
ar = "$ndk_bin/llvm-ar"
EOF
export CC_aarch64_linux_android=$ndk_bin/aarch64-linux-android31-clang
export CXX_aarch64_linux_android=$ndk_bin/aarch64-linux-android31-clang++
export AR_aarch64_linux_android=$ndk_bin/llvm-ar
export ANDROID_PLATFORM=android-31 CARGO_NDK_ANDROID_PLATFORM=android-31

# Debian's older glib-mkenums misreads the flag enums in current libadwaita.
curl -fsSL https://raw.githubusercontent.com/GNOME/glib/main/gobject/glib-mkenums.in \
    | sed -e 's/@VERSION@/2.89.4/' -e 's|@PYTHON@|/usr/bin/python3|' \
    > /usr/bin/glib-mkenums
chmod +x /usr/bin/glib-mkenums

make -f build-aux/android/android.mk android release=1
mkdir -p apks
cp .pixiewood/android/app/build/outputs/apk/release/app-arm64-v8a-release-unsigned.apk \
   apks/Matras-arm64-v8a-release-unsigned.apk
