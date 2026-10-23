project('ru.toxblh.Matras', 'c',
  version: '0.1.0',
  meson_version: '>= 1.8.0',
)

# The application is a Cargo workspace; this meson project exists for Android
# alone, where pixiewood drives one ninja pass over GTK, libadwaita and the
# app. It follows variant A of PORT-PLAN.md 3.7: cargo builds the Rust side
# into a staticlib, and meson links it with a C shim whose `main` is what the
# GTK Android runtime looks up via g_module_symbol. Desktop builds keep using
# cargo directly and never touch this file.

if host_machine.system() != 'android'
  error('this meson project only builds the Android APK; use cargo for desktop builds')
endif

# The cargo step below builds --no-default-features, which is exactly the
# video-less, keyring-less profile; the option exists so the manifest can say
# so out loud and a future desktop meson build could flip it.
if get_option('video').enabled()
  error('video is not supported in the Android build yet (see PORT-PLAN.md 3.3)')
endif

# Resolved through the wrap files pixiewood prepare drops into subprojects/,
# so both build inside the same ninja pass as the app.
gtk_dep = dependency('gtk4', version: '>= 4.14')
adw_dep = dependency('libadwaita-1', version: '>= 1.5')

# aarch64 / x86_64, as spelled by arch/<arch>.cross.
rust_target = host_machine.cpu_family() + '-linux-android'

cargo_script = find_program('build-aux/android/cargo-staticlib.sh')

# build_always_stale because cargo does its own change tracking — ninja only
# needs to know to ask, not what the Rust sources are.
matras_a = custom_target('matras-staticlib',
  output: 'libmatras.a',
  command: [
    cargo_script,
    meson.current_source_dir(),
    rust_target,
    meson.current_build_dir(),
    '@OUTPUT@',
  ],
  console: true,
  build_by_default: true,
  build_always_stale: true,
)

# `main` itself lives in shim.c and forwards to the staticlib's matras_main;
# install: true is what routes the resulting lib<name>.so into jniLibs.
executable('ru.toxblh.Matras', 'build-aux/android/shim.c',
  link_with: matras_a,
  dependencies: [gtk_dep, adw_dep],
  # What the Rust runtime and its crates expect beyond what gtk pulls in:
  # -laaudio is cpal's AAudio backend (min API 26, ours is 31).
  link_args: ['-llog', '-ldl', '-lm', '-laaudio'],
  install: true,
  android_exe_type: 'application',
)
