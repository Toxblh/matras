# Цели для сборки под Android. Подключать из корневого Makefile или звать как
#   make -f build-aux/android/android.mk android
# Образец: Makefile в GeopJr/Tuba.

ANDROID_SDK      ?= $(ANDROID_HOME)
ANDROID_TOOLS    ?= $(abspath .android-tools)
ANDROID_STUDIO   ?= $(ANDROID_TOOLS)/mini-studio
PIXIEWOOD        ?= $(ANDROID_TOOLS)/pixiewood/pixiewood
MANIFEST         := build-aux/android/ru.toxblh.Matras.xml
GEN_MANIFEST     := .pixiewood/android/app/src/main/AndroidManifest.xml

.PHONY: android android-prepare android-generate android-permissions android-build android-sign android-clean

android: android-prepare android-generate android-permissions android-build

android-prepare: android-tools
	$(PIXIEWOOD) prepare $(if $(release),--release,) \
		--sdk=$(ANDROID_SDK) --android-studio=$(ANDROID_STUDIO) $(MANIFEST)

.PHONY: android-tools
android-tools:
	build-aux/android/fetch-tools.sh $(ANDROID_TOOLS)

android-generate:
	$(PIXIEWOOD) generate

# RECORD_AUDIO штатно не добавить: manifest.xsl зашивает список permissions, а хуков
# в pixiewood.xsd нет. Правим СГЕНЕРЁННЫЙ манифест — `build` его не перегенерирует.
android-permissions:
	@grep -q RECORD_AUDIO $(GEN_MANIFEST) || sed -i \
	  's|<uses-permission android:name="android.permission.INTERNET"/>|&\n    <uses-permission android:name="android.permission.RECORD_AUDIO"/>\n    <uses-feature android:name="android.hardware.microphone" android:required="true"/>|' \
	  $(GEN_MANIFEST)
	@grep -q RECORD_AUDIO $(GEN_MANIFEST) && echo "RECORD_AUDIO: ok" || { echo "RECORD_AUDIO НЕ добавлен — проверь, есть ли INTERNET в манифесте (нужен <requires><internet> в metainfo)"; exit 1; }
# Обратный deep-link SSO: браузер возвращает mattermost-dev://…, ToplevelActivity
# передаёт data-URI в GdkContext.open → g_application_open → sso::deliver.
# Схема в манифест штатно не добавляется — тот же приём, что с RECORD_AUDIO.
	@grep -q "mattermost-dev" $(GEN_MANIFEST) || sed -i \
	  's|</activity>|  <intent-filter>\n        <action android:name="android.intent.action.VIEW"/>\n        <category android:name="android.intent.category.DEFAULT"/>\n        <category android:name="android.intent.category.BROWSABLE"/>\n        <data android:scheme="mattermost-dev"/>\n      </intent-filter>\n    </activity>|' \
	  $(GEN_MANIFEST)
	@grep -q "mattermost-dev" $(GEN_MANIFEST) && echo "mattermost-dev deep link: ok" || { echo "deep link НЕ добавлен"; exit 1; }

android-build:
	$(PIXIEWOOD) build
	@ls -la .pixiewood/android/app/build/outputs/apk/*/

# pixiewood не подписывает: release выходит *-release-unsigned.apk.
# ANDROID_KEYSTORE_LOCATION и ANDROID_KEYSTORE_KEY — снаружи.
android-sign:
	mkdir -p apks/
	@for apk in .pixiewood/android/app/build/outputs/apk/release/*.apk; do \
		apksigner sign --ks "$(ANDROID_KEYSTORE_LOCATION)" --ks-pass "env:ANDROID_KEYSTORE_KEY" \
			--in "$$apk" --out "apks/$$(basename "$$apk" | sed 's/unsigned/signed/g')"; \
	done
	@ls -la apks/

android-clean:
	rm -rf .pixiewood
