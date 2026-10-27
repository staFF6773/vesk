# AGENTS.md

## Project

- Vesk is a Rust/Tauri 2 shell that loads Discord's web client and injects the Vencord userscript. It is not an Electron/Vesktop fork.
- Root is a Cargo workspace with one crate, `vesk`, in `src-tauri/`. Use `-p vesk` from the root.
- Default Linux backend is CEF (Chromium, via the pinned Sable runtime fork). `wry` (WebKitGTK) is the alternative feature. README documents behavior and limitations.

## Build and test

Always run Cargo inside the Nix shell; `shell.nix` provides `CEF_PATH`, `LD_LIBRARY_PATH`, `LIBCLANG_PATH`, bindgen args, GIO modules and GSettings schemas that plain `cargo` does not have.

```bash
nix-shell --run "cargo run -p vesk"
nix-shell --run "cargo test -p vesk"
nix-shell --run "cargo test -p vesk <test-name-substring>"
nix-shell --run "cargo build --release -p vesk"
nix-shell --run "cargo check -p vesk --no-default-features --features wry"
```

- CEF is pinned in two places: `cef = "=150.2.1"` in `src-tauri/Cargo.toml` and the CEF 150.0.14 Arch package in `nix/cef-codecs.nix` (wired through `shell.nix`). Keep them compatible. The Arch build is required because the official CEF tarball has `proprietary_codecs=false` and cannot play Discord's H.264/AAC MP4s.
- `cargo fmt --all -- --check` and `cargo clippy -- -D warnings` currently report pre-existing violations. Do not bulk-reformat or fix unrelated files; only keep touched code formatted.
- No CI. Tests are Rust unit tests only. Running the app needs a graphical session, PipeWire, and working GSettings schemas.

## Layout

- `src-tauri/src/main.rs` — app builder, programmatic main window, tray, deep links, session injection, and the large inline `BRIDGE` JS. Most tests are at the bottom.
- `src-tauri/src/cef.rs` — Linux+CEF only: windowing backend, GPU presets, CEF switches, permission/popup policies.
- `src-tauri/src/screen_audio.rs` — PipeWire virtual source and `screen_audio_set` / `screen_audio_list` commands; runs on its own thread.
- `src-tauri/src/session.rs` — Discord token vault (`session.token`, mode 0600).
- `nix/cef-codecs.nix` — fetches the official Arch `cef 150.0.14-1` package, flattens its runtime, writes `archive.json`, and bundles Arch `openh264 2.7`; uses nixpkgs `libjpeg8` for the `libjpeg.so.8` ABI. `shell.nix` exposes it as `CEF_PATH`.
- `src-tauri/resources/vencord.js` — Vencord bundle embedded at compile time with `include_str!`. Update with `./scripts/fetch-vencord.sh`, then rebuild.
- `ui/index.html` is only a fallback. The real window is created in `create_main_window`, not in `tauri.conf.json` (which has no windows).
- New Tauri commands need both an entry in `src-tauri/permissions/screen-audio.toml` and the matching capability in `src-tauri/capabilities/` or they are denied at runtime.

## Conventions and gotchas

- Comments, stderr logs, UI text, and test names are Spanish. Keep that style; identifiers are mostly English.
- Set environment variables and `cef::configure` before creating the Tauri app/webview; they are read at startup. Keep the early `--type=` dispatch for CEF helper processes.
- The Vencord settings reduction replaces an exact minified literal in `main.rs`. If a Vencord update changes it, the patch silently falls back (with an stderr message); `ajustes_solo_informativos` catches it.
- Under CEF the injection script runs before `<html>` exists. The splash relies on a `MutationObserver` watching `document` with `subtree: true`; `splash_de_carga` guards this.
- Native Wayland gets GPU compositing; XWayland falls back to CPU. CEF and GTK must use the same backend (`VESK_GDK_BACKEND=wayland|x11`). Wayland has no reliable window position, so `set_position` and saved window state are unreliable.
- Runtime toggles: `VESK_GPU_PERF=full|safe|0` (safe is default on NVIDIA), `VESK_CEF_ARGS`, `VESK_NO_TRACK=0`, `VESK_DOM_OPTIMIZER=0`, `VESK_MEDIA_OPTIMIZER=0`, `VESK_SCREEN_AUDIO=0`, `VESK_SCREEN_AUDIO_APP=<name>`, `VESK_CLOSE_TO_TRAY=0`. `VESK_TRAY_CYCLE` / `VESK_TRAY_HOLD` automate the hide/show cycle for testing.
- Media playback optimization lives in the `BRIDGE` JS. It pauses off-screen videos/GIFs and must skip `video.srcObject` (calls and screen share); `VESK_MEDIA_OPTIMIZER=0` disables it.
- Do not log or expose the saved Discord token; treat `session.token` as a password.
- CEF codec verification: `codec_probe.rs` from `tauri-runtime-cef` needs H.264/AAC and VP9/Opus samples. The expected result is `mp4Playback` and `webmPlayback` both `plays`. If CEF is updated, re-check the Arch package's `libjpeg.so.8`/`libopenh264.so.8` sonames before touching the derivation.
