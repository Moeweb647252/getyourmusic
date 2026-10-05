# GetYourMusic

Records what your music player plays into one tagged audio file per track.

GetYourMusic follows the system's Now Playing information. That works with any player that reports to Control Center, such as QQ Music, Spotify, Apple Music or a browser. It records the audio sent to an output device, splits it at track changes, and writes FLAC, MP3 or M4A (AAC/ALAC) files. The files carry title, artist, album and cover art.

macOS is implemented. Every OS-specific part sits behind a trait, so other platforms can be added without touching the core.

## Requirements

- macOS 14.2 or later. Direct output recording uses Core Audio process taps.
- Rust nightly with edition 2024. See `rustc --version`; this was developed on 1.100.
- CMake, to build the vendored MediaRemote adapter (`brew install cmake`).
- Command Line Tools. Full Xcode is not needed, because GPUI compiles its shaders at runtime.

## Run

macOS only grants system-audio recording to a bundled app that declares why it needs it. Run the app through the bundle script, not `cargo run`:

```sh
scripts/bundle-macos.sh debug --open      # or: release
```

On the first recording, macOS asks for **System Audio Recording** permission. Ad-hoc signed development builds get a new code signature on every build, so macOS may ask again after a rebuild. For distribution, set `CODESIGN_IDENTITY` to a Developer ID identity, then notarize the app.

As an alternative to output recording, route your player to a loopback driver such as BlackHole and pick it as the source.

### Menu bar / system tray

GetYourMusic adds an icon to the menu bar (macOS) or system tray (Windows, Linux).

- **Recording state:** on macOS, the icon fills in while recording and the elapsed time appears beside it. On Windows and Linux, the elapsed time appears in the tooltip.
- **Menu:** shows what's playing and lets you start or stop recording, open the window, the Library or Settings, or quit.
- **Closing the window:** recording keeps going and the app stays in the menu bar. On macOS the Dock icon hides until the window is reopened.
- **Quitting:** quitting while recording stops the session and finishes encoding queued tracks before exiting.

Both behaviors can be changed in Settings → Appearance. The tray uses [tray-icon](https://crates.io/crates/tray-icon) on all three systems; Linux uses its GTK-free `ksni` backend, which needs a StatusNotifier host. KDE has one built in; GNOME needs the AppIndicator extension. If no tray can be created, closing the window quits as before.

### Debug hooks (debug builds only)

| Variable | Effect |
|---|---|
| `GYM_START_PAGE=library\|settings` | Open on that page |
| `GYM_AUTOSTART=1` | Start recording at launch |
| `RUST_LOG=debug` | More verbose logs |

For example: `open --env GYM_START_PAGE=library target/debug/GetYourMusic.app`.

Logs are in `~/Library/Logs/GetYourMusic`, and Settings → About → Show Logs opens that folder.

## Layout

```
crates/gym-core       Domain model, traits (Platform, NowPlayingSource, AudioCaptureBackend,
                      AudioEncoder, StorageProvider), recording engine, settings, library, tags
crates/gym-media      Portable encoders: FLAC (flacenc), MP3 (LAME)
crates/gym-platform   OS integrations. macOS: MediaRemote adapter, Core Audio capture via cpal,
                      AAC/ALAC via AudioToolbox. Other OSes: an "unsupported" stub
src/                  GPUI Kit app: recorder, library, settings, window shell
locales/app.yml       All user-visible text (rust-i18n; English for now)
packaging/macos       Info.plist template, app icon (AppIcon.svg is the source; AppIcon.icns is generated)
packaging/tray        Tray icon sources; rendered to assets/tray/*.png by scripts/make-icon.sh
scripts/              Bundling (bundle-macos.sh) and icon generation (make-icon.sh)
```

### How tracks are split

1. Captured audio passes through a 3-second delay line before it is committed.
2. Now Playing updates carry the player's elapsed time and timestamp. The engine maps each track start to an exact sample, so a change reported late still splits at the right place.
3. Each cut snaps to the nearest silent gap.
4. Each track's audio is first written to a temporary WAV file.
5. When the track ends, that WAV is trimmed, resampled when the format needs it, encoded, tagged, and moved into storage.

A track that wasn't captured from start to finish is not saved by default. This covers joining mid-track, seeking, stopping early, and audio from another app. Settings → Recording → Incomplete tracks changes that.

## Tests and CI

```sh
cargo test --workspace                                   # unit, encoder round-trip, engine end-to-end
cargo test -p gym-platform --test macos -- --ignored     # needs media playing and audio devices
cargo clippy --workspace --all-targets
```

`.github/workflows/ci.yml` builds, lints and tests on macOS, Windows and Linux. It is manual-only, to avoid spending runner minutes: start it from the Actions tab ("Run workflow"). Only macOS records today; the other systems build the shared code, the tray and the platform stubs.

## Third-party

Track detection uses [mediaremote-adapter](https://github.com/ungive/mediaremote-adapter) (BSD 3-Clause), vendored in `crates/gym-platform/vendor`. The interface uses [GPUI Kit](https://gpui-kit.com) (Apache-2.0).
