#!/bin/sh
# Builds the Rust side of Matras as a staticlib for Android, for the
# custom_target in meson.build. Arguments:
#   $1  source dir (the Cargo workspace root)
#   $2  rust target triple, e.g. aarch64-linux-android
#   $3  meson build dir (holds meson-uninstalled/ and receives cargo's target/)
#   $4  where meson expects libmatras.a
set -eu
SRC="$1"; TARGET="$2"; BUILD="$3"; OUT="$4"

# How the *-sys crates resolve GTK without an installed gtk4.pc (job 04,
# JOURNAL.md): gtk4 and libadwaita through the system-deps escape hatch, the
# rest through the .pc files meson writes for uninstalled subprojects. The -L
# paths these produce are wrong for linking — which is fine, because a
# staticlib links nothing; the final link belongs to meson, which knows the
# real locations.
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_PATH="$BUILD/meson-uninstalled"
export PKG_CONFIG_LIBDIR="$BUILD/meson-uninstalled"
export SYSTEM_DEPS_GTK4_NO_PKG_CONFIG=1
export SYSTEM_DEPS_GTK4_LIB=gtk-4
export SYSTEM_DEPS_LIBADWAITA_1_NO_PKG_CONFIG=1
export SYSTEM_DEPS_LIBADWAITA_1_LIB=adwaita-1

cargo rustc -p matras --lib --release --no-default-features \
  --manifest-path "$SRC/Cargo.toml" \
  --target "$TARGET" \
  --target-dir "$BUILD/cargo" \
  --crate-type staticlib

cp "$BUILD/cargo/$TARGET/release/libmatras.a" "$OUT"
