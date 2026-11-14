# AGENTS.md

## Project

`lilt` is a Rust wrapper around a detached, audio-only `mpv` daemon. The CLI
and the ratatui control panel (`lilt panel`) are two front ends for the same
queue and playback state. The installed binary is `lilt`; a temporary `music`
alias and legacy config/env fallbacks exist for migration.

Linux and macOS are supported. Keep OS-specific process and mount commands in
`src/platform.rs` instead of spreading `cfg` blocks through the rest of the
code. On macOS, `rclone mount` needs FUSE-T (or macFUSE) and the official rclone
build; Homebrew's rclone is compiled without mount support. The spectrum pane
is music-only on both platforms: `parec --monitor-stream` matched to the mpv PID
via `pactl` on Linux, and an `audia-tap --pid` Core Audio tap on macOS. There is
an optional `ffmpeg` + BlackHole fallback via `capture_device`; keep capture
commands in `src/visualizer.rs`.

## Build and test

```sh
cargo fmt
./scripts/check.sh                 # fmt --check, clippy -D warnings, tests
FUZZ_CASES=20000 ./scripts/fuzz.sh # property/fuzz tests
cargo build --release
./scripts/install.sh               # install lilt to ~/.cargo/bin
```

Run `./scripts/check.sh` and a fuzz pass before every commit. Install the
binary after behavior changes so the local `lilt` command matches the source.

## CLI/TUI parity (required)

Every playback or queue action must be available in both the CLI and the TUI.
When adding, changing, or removing one of these actions, update both front ends
in the same change, then update all of:

- `src/main.rs` clap command and dispatch
- `src/player.rs` shared implementation
- `src/tui.rs` key handling and footer hints
- `README.md` CLI command list and TUI key tables
- `completions/lilt.fish` (and the temporary `music.fish` alias)
- `tests/fuzz_cli.rs` for new CLI commands

Current playback/queue mapping:

| Action | CLI | TUI |
| --- | --- | --- |
| play/replace queue | `play SOURCE...` | `o` (library/NTS picker) |
| add to queue | `add SOURCE...` | `a` (library/NTS picker) |
| list queue | `queue` (alias `list`) | Queue pane |
| toggle/pause/resume | `toggle`, `pause`, `resume` | `space` |
| next/previous | `next`, `prev` | `n`, `p`, `j`, `k` |
| stop (keep mpv) | `stop` | `x` |
| mute | `mute [on\|off\|toggle]` | `m` |
| seek | `seek N\|-N\|N%` | `h`/`l` 10s, `H`/`L` 60s, arrows, `t` prompt |
| volume | `volume N\|+N\|-N` | `-`, `+`, arrows |
| shuffle | `shuffle [on\|off\|toggle]` | `s` |
| repeat | `repeat [off\|all\|one]` | `r` |
| jump to queue number | `jump N` | digits then `Enter`/`G` |
| open playing item | `open` | `f` |
| quit mpv | `quit` | `Q` |
| detach TUI | — | `q` |

Exemptions:

- Maintenance commands (`mount`, `unmount`, `logs`, `restart`) stay CLI-only
  unless explicitly added to the TUI.
- TUI-only view features (`/` filter, `v` visualizer, cover art, focus/navigation
  keys) do not need CLI equivalents.

The TUI footer hints are the canonical in-app key reference; there is no help
overlay. Keep them short, accurate, and in sync with the README.

## External sources

- `queue::Input` distinguishes local paths from URLs; URL-only playback must not
  require the rclone mount.
- Keep YouTube audio-only with `--ytdl-format=bestaudio/best`.
- URL covers come from `yt-dlp` in `cover::load_url` and are cached under
  `cache_dir/covers`; playlist entries get per-video thumbnails.
- NTS Radio station/show metadata and artwork come from the public NTS API via
  `curl`; keep resolution and the current-station sidecar in `src/nts.rs`.
  Episode page URLs use the show/episode API for display metadata; playback
  still goes through mpv/yt-dlp.
- Only `Station.live` entries refresh on a 60-second TTL. The TUI refreshes in
  the background; `status`/`queue` refresh stale live metadata before printing.
  Cover keys include the NTS artwork signature so a show change drops the old
  cover and shows the loading placeholder.
- NTS serves live JSON through a 15-minute CDN cache. Keep the unique query
  cache-buster on `/api/v2/live` requests, or polling will keep returning the
  previous show. NTS artwork may be PNG even when cached with a `.jpg` name, so
  decode cover files by content.
- Surface mpv playback failures from `end-file` events and the mpv log.

## Style

- Rust 2024, `rustfmt` defaults, clippy with `-D warnings`.
- Prefer shared logic in `src/player.rs`; CLI and TUI should not duplicate
  mpv command sequences.
- Use `anyhow` for user-facing errors and the existing `Mpv` IPC helper for
  player access.
- Keep commits focused and local; do not push unless asked.
