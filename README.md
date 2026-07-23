# apkrun (prototype)

Zero-friction APK launcher for **Apple Silicon (arm64) desktops**. This CLI
prototype proves the pipeline the eventual Tauri app will wrap:

> provision a self-contained Android SDK → boot an **arm64** AVD (no CPU
> translation, HVF-accelerated) → install an APK → launch it in **phone** or
> **desktop** windowing mode.

## Why arm64 = "zero friction"
An arm64-v8a Android system image on Apple Silicon runs on Hypervisor.framework
with the guest and host both ARM, so there's no instruction translation. The one
unavoidable cost is the first-run SDK + system-image download (~1–2 GB).

## Usage

```bash
./apkrun.sh setup                 # one-time: downloads a private SDK into ./.android-sdk
./apkrun.sh avd                   # create phone + desktop AVDs
./apkrun.sh run app.apk           # boot phone AVD, install, launch  (mobile mode)
./apkrun.sh run app.apk --mode desktop   # freeform / desktop-windowing mode
./apkrun.sh stop                  # shut the emulator down
./apkrun.sh doctor                # what's installed / missing
```

Everything lives under `./.android-sdk` (override with `APKRUN_SDK`), so it never
touches a system-wide Android install.

## Config knobs (env vars)
| var | default | notes |
|-----|---------|-------|
| `APKRUN_API` | `35` | Android 15. Set `36` for Android 16 desktop-windowing images. |
| `APKRUN_IMG_TYPE` | `google_apis` | `google_apis_playstore` adds the Play Store (but locks root). |
| `APKRUN_SDK` | `./.android-sdk` | where the private SDK is provisioned. |

## Known limitations (inherent to any emulator approach)
- **x86-only APKs** need the slower translated image; native/arm64 or universal APKs are frictionless.
- `google_apis` images have Google services but **no Play Store**; Play-Integrity apps may refuse.
- Desktop-windowing flags are best-effort — full freeform behavior wants an **Android 16** desktop image.
- Apps with anti-emulator / DRM checks may not run.

## Next step
Once the pipeline is proven, wrap `apkrun.sh`'s stages in a Tauri (Rust) core
with a drag-drop UI and a phone/desktop toggle.
