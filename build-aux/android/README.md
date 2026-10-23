# Android APK

`sh build-aux/android/ci-build.sh` builds the arm64 APK locally in Docker and writes it
to `apks/Matras-arm64-v8a-release-unsigned.apk`. The first run downloads the
Android SDK/NDK, Rust dependencies, GTK sources, Pixiewood and mini-studio; it
needs several gigabytes of disk space and can take hours. Later runs reuse the
Docker image layers and named Cargo/Gradle volumes. Docker must be available
to the caller. Forgejo runs `ci-linux.sh` inside its Debian job container on
`als-huge`; the runner does not expose a Docker CLI to jobs. Both paths use
the same Android SDK and NDK.

Without signing secrets, Forgejo uploads an unsigned APK. To also upload an
installable signed APK, add these repository Actions secrets at
`Settings → Actions → Secrets`:

| Secret | Value |
|---|---|
| `ANDROID_KEYSTORE_B64` | Base64 of a PKCS#12 signing keystore, on one line |
| `ANDROID_KEYSTORE_PASSWORD` | The keystore password |
| `ANDROID_KEY_ALIAS` | The signing key alias inside the keystore |

Generate the base64 value on macOS with
`base64 < /path/to/keystore.p12 | tr -d '\n' | pbcopy`. Never commit the
keystore or paste its contents into an issue. Android uses the signing key to
authorize app updates, so keep a backup and use the same key for later builds.
The build targets Android arm64, with minSdk 31 and compileSdk 36.

For a container that already has the toolchain, run
`make -f build-aux/android/android.mk android release=1` from the repo root.
`fetch-tools.sh` pins Pixiewood and mini-studio to the commits used for the
working local APK. `.pixiewood`, `.android-tools` and `subprojects` are
generated or downloaded during the build and stay outside Git.
