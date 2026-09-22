# Android APK

`sh build-aux/android/ci-build.sh` builds the arm64 APK locally in Docker and writes it
to `apks/Matras-arm64-v8a-release-unsigned.apk`. The first run downloads the
Android SDK/NDK, Rust dependencies, GTK sources, Pixiewood and mini-studio; it
needs several gigabytes of disk space and can take hours. Later runs reuse the
Docker image layers and named Cargo/Gradle volumes. Docker must be available
to the caller. Forgejo runs `ci-linux.sh` inside its Debian job container on
`als-huge`; the runner does not expose a Docker CLI to jobs. Both paths use
the same Android SDK and NDK.

The APK is unsigned. To install or distribute it, sign it with a private
keystore using Android SDK `apksigner`. Do not commit a keystore. The build
currently targets Android arm64, with minSdk 31 and compileSdk 36.

For a container that already has the toolchain, run
`make -f build-aux/android/android.mk android release=1` from the repo root.
`fetch-tools.sh` pins Pixiewood and mini-studio to the commits used for the
working local APK. `.pixiewood`, `.android-tools` and `subprojects` are
generated or downloaded during the build and stay outside Git.
