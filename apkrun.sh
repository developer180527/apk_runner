#!/usr/bin/env bash
#
# apkrun — zero-friction APK launcher for Apple Silicon (ARM64) desktops.
#
# Prototype CLI that proves the pipeline the future Tauri app will orchestrate:
#   setup  -> provision a self-contained Android SDK (cmdline-tools, emulator,
#             platform-tools, arm64 system image, build-tools) inside this repo
#   avd    -> create a "phone" and a "desktop" AVD from that SDK
#   run    -> boot the right AVD, install an APK, resolve + launch its activity
#   stop   -> shut the emulator down
#   doctor -> report what's present / missing
#
# ARM note: the guest system image is arm64-v8a, so on Apple Silicon it runs on
# Hypervisor.framework with no CPU translation — the "zero friction" path.
#
set -euo pipefail

# ---- config ---------------------------------------------------------------
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SDK="${APKRUN_SDK:-$ROOT/.android-sdk}"          # self-contained SDK lives here
API="${APKRUN_API:-37}"                          # Android 17 = API 37, stable since 2026-06-16
API_TAG="${APKRUN_API_TAG:-37.0}"                # SDK package tag is dotted: android-37.0
# Android 17 images are all 16 KB-page (_ps16k) variants. Use a *userdebug* one so
# `adb root` works and Magisk can patch it:
#   google_apis_ps16k           -> userdebug, Google services, NO Play (rootable) [default]
#   google_apis_playstore_ps16k -> user build, Play Store              (LOCKED, not rootable)
IMG_TYPE="${APKRUN_IMG_TYPE:-google_apis_ps16k}"
ABI="arm64-v8a"
BUILD_TOOLS="${APKRUN_BUILD_TOOLS:-37.0.0}"
SYSIMG="system-images;android-${API_TAG};${IMG_TYPE};${ABI}"

PHONE_AVD="apkrun_phone"
DESKTOP_AVD="apkrun_desktop"
PHONE_DEVICE="pixel_7"                            # phone-shaped profile
DESKTOP_DEVICE="10.1in WXGA (Tablet)"            # big freeform-friendly profile

# cmdline-tools package (Java tool; the "mac" build runs fine on arm64)
CLT_VER="11076708"
CLT_URL="https://dl.google.com/android/repository/commandlinetools-mac-${CLT_VER}_latest.zip"

export ANDROID_SDK_ROOT="$SDK"
export ANDROID_HOME="$SDK"
BIN_CLT="$SDK/cmdline-tools/latest/bin"
BIN_PLAT="$SDK/platform-tools"
BIN_EMU="$SDK/emulator"
BIN_BT="$SDK/build-tools/$BUILD_TOOLS"
PATH="$BIN_CLT:$BIN_PLAT:$BIN_EMU:$BIN_BT:$PATH"

# ---- pretty logging -------------------------------------------------------
c() { printf "\033[%sm%s\033[0m" "$1" "$2"; }
info() { echo "$(c '1;34' '›') $*"; }
ok()   { echo "$(c '1;32' '✓') $*"; }
warn() { echo "$(c '1;33' '!') $*" >&2; }
die()  { echo "$(c '1;31' '✗') $*" >&2; exit 1; }

need_java() { command -v java >/dev/null || die "Java not found. Install a JDK (e.g. brew install openjdk@21)."; }

# ---- setup ----------------------------------------------------------------
cmd_setup() {
  need_java
  [[ "$(uname -m)" == "arm64" ]] || warn "Host is $(uname -m), not arm64 — arm64 images won't be accelerated."
  mkdir -p "$SDK"

  if [[ ! -x "$BIN_CLT/sdkmanager" ]]; then
    info "Downloading Android command-line tools…"
    local tmp="$SDK/clt.zip"
    curl -fL# "$CLT_URL" -o "$tmp"
    info "Unpacking into cmdline-tools/latest…"
    rm -rf "$SDK/cmdline-tools/latest" "$SDK/cmdline-tools/_x"
    mkdir -p "$SDK/cmdline-tools"
    unzip -q "$tmp" -d "$SDK/cmdline-tools/_x"
    mv "$SDK/cmdline-tools/_x/cmdline-tools" "$SDK/cmdline-tools/latest"
    rm -rf "$SDK/cmdline-tools/_x" "$tmp"
    ok "cmdline-tools installed."
  else
    ok "cmdline-tools already present."
  fi

  info "Accepting licenses…"
  yes | sdkmanager --sdk_root="$SDK" --licenses >/dev/null || true

  info "Installing platform-tools, emulator, build-tools, and $SYSIMG …"
  info "(first run is a large download, ~1–2 GB — this is the one unavoidable cost)"
  sdkmanager --sdk_root="$SDK" \
    "platform-tools" "emulator" "build-tools;${BUILD_TOOLS}" \
    "platforms;android-${API_TAG}" "$SYSIMG"
  ok "SDK provisioned at $SDK"
}

# ---- avd creation ---------------------------------------------------------
create_avd() {  # $1=name $2=device-profile
  local name="$1" device="$2"
  if avdmanager list avd 2>/dev/null | grep -q "Name: $name"; then
    ok "AVD '$name' already exists."; return
  fi
  info "Creating AVD '$name' (device: $device)…"
  echo "no" | avdmanager create avd -n "$name" -k "$SYSIMG" -d "$device" --force
  # avdmanager defaults RAM to 256MB — far too low for Android 17. Bump it so the
  # guest actually boots (independent of CPU acceleration).
  local cfg="${ANDROID_AVD_HOME:-$HOME/.android/avd}/$name.avd/config.ini"
  if [[ -f "$cfg" ]]; then
    grep -vE "^(hw.ramSize|vm.heapSize)=" "$cfg" > "$cfg.tmp" && mv "$cfg.tmp" "$cfg"
    printf 'hw.ramSize=2048\nvm.heapSize=512\n' >> "$cfg"
  fi
  ok "AVD '$name' created (2GB RAM)."
}

cmd_avd() {
  [[ -x "$BIN_CLT/avdmanager" ]] || die "SDK missing — run:  $0 setup"
  create_avd "$PHONE_AVD" "$PHONE_DEVICE"
  create_avd "$DESKTOP_AVD" "$DESKTOP_DEVICE"
}

# ---- boot helpers ---------------------------------------------------------
emu_running() { command -v adb >/dev/null || return 1; adb devices 2>/dev/null | grep -qE '^emulator-[0-9]+\s+device$'; }

boot_avd() {  # $1=avd name
  local name="$1"
  if emu_running; then ok "Emulator already running."; return; fi
  info "Booting AVD '$name' (arm64, HVF-accelerated)…"
  # -no-snapshot keeps the prototype deterministic; drop it later for fast resume.
  nohup emulator -avd "$name" -no-snapshot -gpu auto \
      >"$ROOT/.emulator.log" 2>&1 &
  info "Waiting for device…"
  adb wait-for-device
  info "Waiting for full boot…"
  until [[ "$(adb shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" == "1" ]]; do
    sleep 2
  done
  adb shell input keyevent 82 >/dev/null 2>&1 || true   # dismiss keyguard
  ok "Android booted."
}

# ---- desktop windowing ----------------------------------------------------
enable_desktop_mode() {
  info "Enabling desktop / freeform windowing (best-effort)…"
  adb shell settings put global development_settings_enabled 1 || true
  adb shell settings put global enable_freeform_support 1 || true
  adb shell settings put global force_desktop_mode_on_external_displays 1 || true
  # Android 16/17 desktop-windowing debug flag (needs a build that honors it):
  adb shell settings put global force_desktop_mode 1 2>/dev/null || true
  ok "Desktop-mode settings applied (Android 17 API 37 adaptive layouts make apps free-resizing)."
}

# ---- root -----------------------------------------------------------------
# Two layers of root:
#   1. adbd root  — `adb root` on a userdebug image; instant root *shell*.
#   2. Magisk     — patches the system image ramdisk (rootAVD) for app-visible
#                   systemless `su`, so root-requiring apps/games work.
ROOTAVD_URL="https://github.com/newbit1/rootAVD"
ROOTAVD_DIR="$SDK/rootAVD"

img_ramdisk() { echo "$SDK/system-images/android-${API_TAG}/${IMG_TYPE}/${ABI}/ramdisk.img"; }

root_status() {  # prints: none | adbd | magisk
  emu_running || { echo "none"; return; }
  if adb shell su -c id 2>/dev/null | grep -q 'uid=0'; then echo "magisk"; return; fi
  if [[ "$(adb shell id -u 2>/dev/null | tr -d '\r')" == "0" ]]; then echo "adbd"; return; fi
  echo "none"
}

enable_adbd_root() {
  [[ "$IMG_TYPE" == *playstore* ]] && \
    die "Play Store images are locked (user build) — use a google_apis(_ps16k) image for root."
  info "Requesting adbd root (userdebug image)…"
  adb root >/dev/null 2>&1 || true
  adb wait-for-device
  if [[ "$(adb shell id -u 2>/dev/null | tr -d '\r')" == "0" ]]; then
    ok "adbd root active (root shell)."
  else
    warn "adb root did not elevate — is this a userdebug image?"
  fi
}

install_magisk() {
  local ramdisk; ramdisk="$(img_ramdisk)"
  [[ -f "$ramdisk" ]] || die "System image ramdisk not found: $ramdisk  (run: $0 setup)"
  if emu_running; then
    warn "Emulator is running. rootAVD patches the image offline — stopping it first."
    cmd_stop
  fi
  if [[ ! -d "$ROOTAVD_DIR" ]]; then
    command -v git >/dev/null || die "git required to fetch rootAVD."
    info "Fetching rootAVD (Magisk patcher)…"
    git clone --depth 1 "$ROOTAVD_URL" "$ROOTAVD_DIR"
  fi
  info "Patching $IMG_TYPE/$ABI ramdisk with Magisk…"
  ( cd "$ROOTAVD_DIR" && ./rootAVD.sh "system-images/android-${API_TAG}/${IMG_TYPE}/${ABI}/ramdisk.img" )
  ok "Magisk patched. Cold-boot the AVD to activate (snapshots would discard it)."
}

cmd_root() {
  local mode="${1:-magisk}"
  [[ -x "$BIN_PLAT/adb" ]] || die "SDK missing — run:  $0 setup"
  case "$mode" in
    adbd)   enable_adbd_root;;
    magisk) install_magisk;;
    status) echo "root: $(root_status)";;
    *) die "Usage: $0 root [adbd|magisk|status]";;
  esac
}

# ---- apk package/activity resolution --------------------------------------
apk_package() {  # $1=apk -> prints package name
  local apk="$1" pkg=""
  if command -v aapt2 >/dev/null; then
    pkg="$(aapt2 dump packagename "$apk" 2>/dev/null || true)"
  fi
  echo "$pkg"
}

launch_apk() {  # $1=apk
  local apk="$1"
  [[ -f "$apk" ]] || die "APK not found: $apk"
  info "Installing $(basename "$apk")…"

  local before after pkg
  before="$(adb shell pm list packages -3 2>/dev/null | tr -d '\r' | sort)"
  adb install -r -g "$apk"
  after="$(adb shell pm list packages -3 2>/dev/null | tr -d '\r' | sort)"

  # Prefer aapt2; fall back to diffing the 3rd-party package list.
  pkg="$(apk_package "$apk")"
  if [[ -z "$pkg" ]]; then
    pkg="$(comm -13 <(echo "$before") <(echo "$after") | sed 's/^package://' | head -n1)"
  fi
  [[ -n "$pkg" ]] || die "Could not determine package name for $apk"
  ok "Package: $pkg"

  local comp
  comp="$(adb shell cmd package resolve-activity --brief -c android.intent.category.LAUNCHER "$pkg" 2>/dev/null | tr -d '\r' | tail -n1)"
  if [[ "$comp" == */* ]]; then
    info "Launching $comp …"
    adb shell am start -n "$comp" >/dev/null
  else
    info "No LAUNCHER activity resolved; using monkey to start $pkg …"
    adb shell monkey -p "$pkg" -c android.intent.category.LAUNCHER 1 >/dev/null
  fi
  ok "Launched. Emulator log: $ROOT/.emulator.log"
}

# ---- run ------------------------------------------------------------------
cmd_run() {
  local apk="" mode="phone"
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --mode) mode="$2"; shift 2;;
      --mode=*) mode="${1#*=}"; shift;;
      -*) die "Unknown flag: $1";;
      *) apk="$1"; shift;;
    esac
  done
  [[ -n "$apk" ]] || die "Usage: $0 run <app.apk> [--mode phone|desktop]"
  [[ -x "$BIN_EMU/emulator" ]] || die "SDK missing — run:  $0 setup && $0 avd"

  case "$mode" in
    phone)   boot_avd "$PHONE_AVD";;
    desktop) boot_avd "$DESKTOP_AVD"; enable_desktop_mode;;
    *) die "mode must be 'phone' or 'desktop'";;
  esac
  launch_apk "$apk"
}

# ---- stop / doctor --------------------------------------------------------
cmd_stop() {
  if emu_running; then info "Stopping emulator…"; adb emu kill || adb -e emu kill || true; ok "Stopped."
  else ok "No emulator running."; fi
}

cmd_doctor() {
  echo "SDK root : $SDK"
  echo "API level: $API   image: $SYSIMG"
  for t in sdkmanager avdmanager adb emulator aapt2; do
    if command -v "$t" >/dev/null; then echo "  $(c '1;32' '✓') $t -> $(command -v $t)";
    else echo "  $(c '1;31' '✗') $t (run: $0 setup)"; fi
  done
  echo "AVDs:"; avdmanager list avd 2>/dev/null | grep 'Name:' || echo "  (none — run: $0 avd)"
  echo "Emulator running: $(emu_running && echo yes || echo no)"
  echo "Root status     : $(root_status)   (none | adbd | magisk)"
}

# ---- dispatch -------------------------------------------------------------
usage() {
  cat <<EOF
apkrun — run APKs on Apple Silicon with zero friction

  $0 setup                         provision self-contained SDK (one-time, large DL)
  $0 avd                           create phone + desktop AVDs
  $0 run <app.apk> [--mode M]      boot, install, launch  (M = phone | desktop)
  $0 root [adbd|magisk|status]     enable root (adbd shell / Magisk systemless / report)
  $0 stop                          shut the emulator down
  $0 doctor                        show what's installed / missing

Android 17 (API 37). Root needs a *userdebug* image (google_apis or aosp), not Play Store.
Env overrides: APKRUN_API=$API  APKRUN_IMG_TYPE=$IMG_TYPE  APKRUN_SDK=$SDK
EOF
}

case "${1:-}" in
  setup)  shift; cmd_setup "$@";;
  avd)    shift; cmd_avd "$@";;
  run)    shift; cmd_run "$@";;
  root)   shift; cmd_root "$@";;
  stop)   shift; cmd_stop "$@";;
  doctor) shift; cmd_doctor "$@";;
  ""|-h|--help|help) usage;;
  *) die "Unknown command: $1  (try: $0 --help)";;
esac
