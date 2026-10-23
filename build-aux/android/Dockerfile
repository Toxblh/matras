# Окружение сборки Matras под Android.
#
# ОБЯЗАТЕЛЬНО linux/amd64: pixiewood в prepare/android.cross жёстко зашивает путь
# `toolchains/llvm/prebuilt/linux-x86_64`, а linux-aarch64 сборок NDK Google не
# выпускает. На Apple Silicon это эмуляция — см. заметку про Rosetta в конце.
#
# Этот же образ должен использоваться в CI, чтобы «у меня собиралось» и «на CI
# собирается» означали одно и то же.
#
# Первая версия ставила Rust через rustup и собирала cargo-ndk из исходников.
# Под эмуляцией установщик rustup стартовал 118 секунд, а на распаковке
# компонентов встал больше чем на полчаса — при том что 1.5 ГБ NDK приехали за
# 52 секунды, то есть упирается в CPU, а не в сеть. Отсюда две правки:
#   * базовый образ уже с Rust — rustup не запускается вообще;
#   * cargo-ndk выкинут: он только расставляет переменные окружения для cc и
#     линкера, а его сборка из исходников — это ~100 крейтов под эмуляцией.
#     Те же переменные прописаны здесь руками, ниже.
FROM --platform=linux/amd64 rust:1-bookworm

ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8

# --- Слой 1: системные пакеты -------------------------------------------------
# Первая группа — то, что нужно самому pixiewood (perl-модули, appstream, java).
# Вторая — сборка GTK-стека из subprojects.
RUN apt-get update && apt-get install -y --no-install-recommends \
      libglib-perl libglib-object-introspection-perl libipc-run-perl \
      libjson-perl libset-scalar-perl libxml-libxml-perl libxml-libxslt-perl \
      gir1.2-appstream-1.0 openjdk-17-jdk-headless \
      build-essential glslc gobject-introspection libglib2.0-dev-bin \
      libxml2-utils ninja-build sassc gettext pkg-config cmake \
      git curl wget unzip ca-certificates python3 python3-pip xz-utils \
    && rm -rf /var/lib/apt/lists/*

# --- Слой 2: meson ------------------------------------------------------------
# android_exe_type появился в meson 1.8, починка его для Rust (pkg.executable) —
# в 1.11. В Debian заметно старше, поэтому из pip.
RUN pip3 install --break-system-packages --no-cache-dir 'meson>=1.11'

# --- Слой 3: Android SDK + NDK ------------------------------------------------
# NDK пинится той же версией, что в CI самой GTK, — чтобы наши грабли совпадали
# с чужими, а не были уникальными.
ENV ANDROID_HOME=/opt/android-sdk \
    ANDROID_SDK_ROOT=/opt/android-sdk \
    NDK_VERSION=27.2.12479018 \
    CMDLINE_TOOLS=13114758

RUN mkdir -p $ANDROID_HOME/cmdline-tools \
 && curl -fsSL -o /tmp/cmdline.zip \
      "https://dl.google.com/android/repository/commandlinetools-linux-${CMDLINE_TOOLS}_latest.zip" \
 && unzip -q /tmp/cmdline.zip -d /tmp \
 && mv /tmp/cmdline-tools $ANDROID_HOME/cmdline-tools/latest \
 && rm /tmp/cmdline.zip

ENV PATH="$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$PATH"

# compileSdk/targetSdk у pixiewood зашиты в 36, minSdk 31.
RUN yes | sdkmanager --licenses > /dev/null 2>&1 || true
RUN sdkmanager --install "platform-tools" "platforms;android-36" \
                         "build-tools;36.0.0" "ndk;${NDK_VERSION}" > /dev/null

ENV ANDROID_NDK_HOME=/opt/android-sdk/ndk/27.2.12479018 \
    ANDROID_NDK_ROOT=/opt/android-sdk/ndk/27.2.12479018 \
    NDK_BIN=/opt/android-sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin

# --- Слой 4: android-таргеты Rust --------------------------------------------
# Единственное, что здесь остаётся от rustup: два std-компонента.
ENV RUSTUP_MAX_RETRIES=10
RUN rustup target add aarch64-linux-android x86_64-linux-android

# --- Слой 5: кросс-настройка cargo (вместо cargo-ndk) -------------------------
# Прошлая версия писала config.toml через heredoc ВНУТРИ RUN — Docker так не умеет,
# инструкция разваливалась и весь этот блок в образ не попадал: ни CC_*, ни NDK в
# PATH. Проявилось как exit 127 на `aarch64-linux-android21-clang` — cc-rs не нашёл
# переменных, откатился на своё умолчание (API 21) и не нашёл его в PATH.
# Обёртка android21 в NDK, к слову, есть — дело было только в PATH.
ENV CARGO_HOME=/opt/cargo
ENV NDK_BIN=/opt/android-sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin
ENV PATH="/opt/android-sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin:${PATH}"

# API 31 выбран не случайно: ровно его pixiewood зашивает в имя clang-враппера в
# prepare/android.cross. Расходиться нельзя — получим два разных минимальных API
# в одном .so.
RUN mkdir -p /opt/cargo && printf '%s\n' \
  '[target.aarch64-linux-android]' \
  'linker = "/opt/android-sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android31-clang"' \
  'ar = "/opt/android-sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar"' \
  '' \
  '[target.x86_64-linux-android]' \
  'linker = "/opt/android-sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android31-clang"' \
  'ar = "/opt/android-sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar"' \
  > /opt/cargo/config.toml

# cc-rs ищет CC_<target> ЧЕРЕЗ ДЕФИСЫ и только потом через подчёркивания.
# Задаём оба написания и абсолютными путями — тогда PATH перестаёт иметь значение.
# ANDROID_PLATFORM / CARGO_NDK_ANDROID_PLATFORM читает opusic-sys (opus 0.4).
ENV CC_aarch64-linux-android=/opt/android-sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android31-clang \
    CXX_aarch64-linux-android=/opt/android-sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android31-clang++ \
    AR_aarch64-linux-android=/opt/android-sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar \
    CC_aarch64_linux_android=/opt/android-sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android31-clang \
    CXX_aarch64_linux_android=/opt/android-sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android31-clang++ \
    AR_aarch64_linux_android=/opt/android-sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar \
    ANDROID_PLATFORM=android-31 \
    CARGO_NDK_ANDROID_PLATFORM=android-31

# --- Проверка -----------------------------------------------------------------
RUN set -e; \
    echo "meson  $(meson --version)"; \
    echo "ninja  $(ninja --version)"; \
    echo "rustc  $(rustc -V)"; \
    echo "java   $(java -version 2>&1 | head -1)"; \
    command -v aarch64-linux-android31-clang > /dev/null || { echo "нет clang в PATH"; exit 1; }; \
    test -n "$CC_aarch64_linux_android" || { echo "CC_* не задан"; exit 1; }; \
    test -f /opt/cargo/config.toml || { echo "нет cargo config"; exit 1; }; \
    rustc --print target-list | grep -qx aarch64-linux-android || { echo "нет таргета"; exit 1; }; \
    for m in Glib Glib::Object::Introspection IPC::Run JSON Set::Scalar XML::LibXML XML::LibXSLT Archive::Tar; do \
      perl -M"$m" -e1 || { echo "нет perl-модуля $m"; exit 1; }; done; \
    echo "OK: окружение собрано"

# Свежая libadwaita помечает флаговые enum'ы как `} G_GNUC_FLAG_ENUM Name;`,
# а glib-mkenums 2.74 из Debian захватывает МАКРОС вместо имени enum'а
# (@enum_name@ становится «g___gnuc__flag__enum») — тип пропадает из
# adw-enums.h и сборка падает на ADW_TYPE_TAB_VIEW_SHORTCUTS. Скрипт
# самодостаточный python — берём его из glib main.
# ПЕРЕЗАПИСЫВАЕМ /usr/bin/glib-mkenums, а не кладём тень в /usr/local/bin:
# gnome-модуль meson берёт путь из pkg-config-переменной системного glib-2.0,
# минуя PATH, — тень он не видит (проверено: build.ninja пиннил /usr/bin).
RUN curl -fsSL https://raw.githubusercontent.com/GNOME/glib/main/gobject/glib-mkenums.in \
      | sed -e 's/@VERSION@/2.89.4/' -e 's|@PYTHON@|/usr/bin/python3|' > /usr/bin/glib-mkenums \
 && chmod +x /usr/bin/glib-mkenums

# Самопроверка тем же входом, на котором сломался старый, по тому же пути,
# каким его зовёт meson. Проверяем именно имя, не значения.
RUN printf '%s\n' \
      'typedef enum /*< flags >*/ {' \
      '  TEST_FLAG_A = 1 << 0,' \
      '} G_GNUC_FLAG_ENUM TestFlags;' > /tmp/flagtest.h \
 && /usr/bin/glib-mkenums --vhead 'NAME=@enum_name@' /tmp/flagtest.h | grep -qx 'NAME=test_flags' \
 && echo "OK: glib-mkenums понимает G_GNUC_FLAG_ENUM" \
 && rm /tmp/flagtest.h

# Воркер запускает контейнер через `bash -lc`, а /etc/profile переписывает PATH и
# выбрасывает /usr/local/cargo/bin — отсюда `cargo: command not found` в задании при
# том, что при сборке образа rustc находился. Симлинки в /usr/local/bin переживают
# любой профиль.
RUN set -e; \
    CARGO_BIN="$(command -v cargo || true)"; \
    [ -n "$CARGO_BIN" ] || CARGO_BIN="$(ls /usr/local/cargo/bin/cargo /opt/cargo/bin/cargo 2>/dev/null | head -1)"; \
    if [ -z "$CARGO_BIN" ]; then \
      export RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo; \
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable; \
      CARGO_BIN=/usr/local/cargo/bin/cargo; \
    fi; \
    for f in "$(dirname "$CARGO_BIN")"/*; do ln -sf "$f" /usr/local/bin/; done; \
    /usr/local/bin/rustup target add aarch64-linux-android

# Проверка ТЕМ ЖЕ шеллом, которым запускает воркер. Именно её отсутствие стоило прогона.
RUN bash -lc 'cargo -V && rustc -V && rustc --print target-list | grep -qx aarch64-linux-android' \
 && echo "OK: cargo доступен из login-шелла"

RUN mkdir -p /work/matras
WORKDIR /work
