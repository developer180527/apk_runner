# Androlon Suite Architecture

*Status: draft for review — 2026-07-23*

## Principle

Androlon is not one app; it is a **suite of small, single-purpose apps** sharing
one engine. Each mini-app does one job, owns its own windows through OS-native
APIs, and talks to shared state through a tiny IPC layer. The engine stays
portable Rust; only the thin chrome of each mini-app is per-OS.

This extends the philosophy that already works for us: every appified Android
app is its own process. The same isolation, lifecycle, and Dock identity
benefits apply to our *own* tools.

## The processes

```
                    ┌────────────────────────────────────────┐
                    │  androlon-runtimed  (the daemon)       │
                    │  owns the emulator lifecycle:          │
                    │  boot on demand · refcount clients ·   │
                    │  shutdown when idle · device state     │
                    └───────▲────────▲───────────▲───────────┘
                            │ IPC (unix socket, line-JSON)
        ┌───────────────────┼────────────────┼───────────────────┐
        │                   │                │                   │
┌───────┴────────┐  ┌───────┴───────┐  ┌─────┴─────────┐  ┌──────┴───────┐
│ Androlon.app   │  │ Installer.app │  │ Player        │  │ androlon-ctl │
│ (Hub/Library)  │  │ owns .apk     │  │ (invisible:   │  │ (CLI, power  │
│ browse, appify,│  │ association;  │  │ appified      │  │ users + CI)  │
│ uninstall,     │  │ install       │  │ bundles exec  │  └──────────────┘
│ keymaps,       │  │ wizard        │  │ it; one pane, │
│ settings       │  └───────────────┘  │ input, audio) │
└────────────────┘                     └───────────────┘
```

- **androlon-runtimed** — the one NEW concept. A background daemon that owns
  the Android runtime: boots it (headless) when the first client asks, hands
  out device state, refcounts attached players, stops the emulator when the
  last one leaves (configurable linger). Menu-bar presence on macOS later.
  Every other process stops shelling `adb`/`emulator` ad hoc and asks the
  daemon instead. Single source of truth; no more "is something else booting
  it right now?" races.
- **Player** (`androlon-player`) — what an appified bundle executes. Today
  this is `androlon-app --app`; it becomes its own slim binary: one Coherence
  pane + input + audio, nothing else. No management code linked in at all.
- **Installer** (`Androlon Installer.app`) — owns the `.apk` file association.
  Shows the install wizard (icon, details, destination), runs install+appify,
  launches the result. Small enough to justify a fully native shell early.
- **Hub** (`Androlon.app`) — the library: installed apps with icons,
  launch/appify/uninstall, keymap management, runtime settings. The "front
  door" users open on purpose.
- **androlon-ctl** — stays as-is: CLI over the same engine crates, useful for
  scripting and CI.

## The layers (what is shared vs. per-OS)

| Layer | Crate(s) | Portable? |
|---|---|---|
| Engine: SDK/AVD, adb, appify | `androlon-core` | yes |
| Streaming: scrcpy, decode, control | `androlon-stream` | yes (+ macOS fast paths) |
| **IPC protocol + client** | `androlon-ipc` *(new)* | yes |
| **Daemon** | `androlon-runtimed` *(new)* | yes |
| Player pane (SDL + AVLayer) | `androlon-player` *(split out)* | yes (macOS fast path) |
| Mini-app chrome (windows, dialogs, menus) | per mini-app | **per-OS by design** |

Native chrome strategy is **progressive**: every mini-app starts as the thin
shell we can build fastest (SDL/ImGui today), and is promoted to a native
shell (SwiftUI/AppKit on macOS) one at a time, smallest first — the engine
API doesn't change underneath. Native shells call the Rust engine through a
small C FFI (`androlon-ffi`) or drive the daemon over IPC directly (preferred:
the IPC boundary *is* the FFI, no bindings needed).

## IPC: deliberately boring

Unix domain socket at `~/.androlon/runtimed.sock`, newline-delimited JSON,
hand-rolled (no serde — engine stays dep-light). Requests are verbs:

```
{"req":"status"}                     → {"ok":{"booted":true,"device":"emulator-5554","clients":2}}
{"req":"ensure-booted"}              → blocks until adb ready, refcounts caller
{"req":"release"}                    → decrement; daemon may schedule shutdown
{"req":"installed-apps"}             → [{"package":"…","label":"…"} …]
{"req":"install","apk":"/path"}      → progress lines, then ok
{"req":"uninstall","package":"…"}    → ok
```

The daemon self-starts: any client that can't connect spawns it
(`androlon-runtimed --daemonize`) and retries. No launchd dependency in v1
(macOS LaunchAgent is a distribution-time nicety).

## macOS packaging

One download, suite inside — the Xcode model:

```
Androlon.app                          ← the Hub (what users see in /Applications)
└── Contents/
    ├── MacOS/Androlon                ← hub binary
    ├── Library/
    │   ├── Androlon Installer.app    ← registers the .apk association
    │   └── androlon-runtimed
    ├── Helpers/androlon-player       ← appified bundles reference this ONE
    │                                    binary (no more per-app binary copies)
    └── Resources/scrcpy-server, …
```

Appified bundles shrink to Info.plist + icon + a 2-line exec trampoline to the
shared player — fixing today's wart where every generated app embeds a full
copy of the binary (and goes stale on every rebuild).

## Migration order (each step keeps everything working)

1. `androlon-ipc` + `androlon-runtimed` (status / ensure-booted / release /
   installed-apps) — Hub + Player become clients. *Boot ownership lands here
   for free: first launch boots the emulator headless; no Android Studio.*
2. Split `androlon-player` out of androlon-app; appify generates trampoline
   bundles pointing at the shared player.
3. Installer becomes its own app owning the `.apk` association (ImGui shell
   first, this is the first one to go native).
4. Hub gains the Library view (installed apps via daemon, launch/appify/
   uninstall/keymap per row).
5. Promote shells to native chrome, smallest first: Installer → Hub.
   Player windows are already effectively native (CAMetalLayer content).

## Non-goals (for now)

- No plugin system, no third-party extension points.
- No cross-machine IPC; the socket is local and mode-0600.
- Linux/Windows suite layouts follow the same process model later; nothing in
  the design is macOS-specific except the bundle packaging.
