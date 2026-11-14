# Lilt

A Rust wrapper around [mpv](https://mpv.io/) for playing a music library that
lives behind an [rclone](https://rclone.org/) mount.

It does three things:

- mounts the library (`rclone mount`, read-only) when asked,
- keeps one detached, audio-only mpv process running with an IPC socket,
- forwards commands (`play`, `next`, `pause`, `seek`, …) to that process from
  any terminal, with a multi-pane ratatui control panel.

Because mpv stays alive in the background, playback continues after the
terminal that started it is closed, and the next command works from anywhere.
If [`mpv-mpris`](https://github.com/hoyon/mpv-mpris) is installed, desktop media
keys and MPRIS panels work too.

## Requirements

- `mpv`, `rclone`
- `fzf` for `lilt browse`
- `yt-dlp` for YouTube playback and thumbnail covers
- `curl` for NTS Radio metadata and mixtape artwork
- `tail` for `lilt logs -f`
- optional: `mpv-mpris` for hardware media keys (Linux)

Linux:

- `parec` and `pactl` (pulseaudio-utils) for the spectrum visualizer
- `mountpoint` and `fusermount3` (util-linux) for mount detection/unmount
- `setsid` and `XDG_RUNTIME_DIR` for the detached player
- `xdg-open` (xdg-utils) for `lilt open`; `dbus-send` (dbus) is optional

macOS:

- FUSE-T (`brew install --cask fuse-t`) or an enabled macFUSE install for
  `rclone mount`
- the official rclone build from rclone.org: Homebrew's rclone is compiled
  without FUSE support and refuses `rclone mount` on macOS
- if macFUSE is installed but not enabled, rclone prefers its
  `/usr/local/lib/libfuse.2.dylib` over FUSE-T; remove or rename that library
  (or uninstall macFUSE) so rclone falls back to `libfuse-t.dylib`
- the spectrum visualizer taps the mpv process with `audia-tap`. Install it with
  `brew tap Bibhav48/tap && brew trust bibhav48/tap && brew install --cask audia-tap`,
  or build it from source with `./scripts/install-audia-tap.sh` (Command Line
  Tools only, no Xcode, no sudo). It is ad-hoc signed, so macOS may need
  one-time approval in System Settings; launch its agent once with
  `open -a audia-tap --args --agent`, then grant System Audio Recording.
  Rebuilding changes the ad-hoc signature, so macOS may ask again.
- optional fallback: `ffmpeg` and BlackHole 2ch
  (`brew install --cask blackhole-2ch`), plus a Multi-Output device that
  includes BlackHole and the output you listen on; select it with
  `capture_device`

The platform-specific process and mount helpers live in `src/platform.rs`.

## Build and install

```sh
cargo build --release
./target/release/lilt --help

# Install `lilt` (into ~/.cargo/bin, or ~/.local/bin when that is not on PATH)
./scripts/install.sh

# Format, lint, and test
./scripts/check.sh

# Property/fuzz tests (bigger case count)
./scripts/fuzz.sh
```

`scripts/fuzz.sh` exercises random CLI parses and executions, TOML config
parsing, path resolution, playlist building over random directory trees, mpv
IPC replies and status parsing, and ratatui rendering at random terminal sizes.
Use `FUZZ_CASES=50000 ./scripts/fuzz.sh` for a longer run.

On macOS, `brew install mpv fzf`, install FUSE-T and `audia-tap` (see
Requirements), and put the official rclone binary ahead of Homebrew's on `PATH`
(for example in `~/.local/bin`). The Homebrew rclone build has no `mount`
support, so `lilt mount`/`play` would otherwise fail.

## Usage

`lilt mount` mounts the library in the background and returns once it is ready;
`lilt unmount` stops the mount. Mount logs go to `~/.cache/lilt/rclone.log`.
Unmount quits mpv only when a library track is playing or queued, so URL-only
playback keeps running. `lilt` or `lilt help` shows the command list.

```
Library
  lilt mount                    Mount in the background
  lilt unmount                  Unmount; quit mpv only for library playback
  lilt status                   Mount state + now playing

Playback
  lilt play [SOURCE...] [--shuffle]
                                 Replace the queue with paths, URLs, or NTS
                                 stations. No SOURCE = whole library shuffled
  lilt add SOURCE...            Append local paths, URLs, or NTS stations
  lilt browse                   Pick artists, albums, or NTS with fzf
  lilt queue                    List the queue, current track marked
  lilt open                     Open the playing URL or reveal the local file
  lilt toggle | pause | resume
  lilt next | prev | stop
  lilt mute [on|off|toggle]
  lilt jump N
  lilt volume N|+N|-N
  lilt seek N|-N|N%
  lilt shuffle [on|off|toggle]
  lilt repeat [off|all|one]
  lilt panel                    Full-screen control panel
  lilt quit                     Stop and exit mpv
  lilt restart                  Quit mpv and unmount the library
  lilt logs [-f]                Tail rclone/mpv logs
```

`lilt queue` prints one numbered track per line, with `>` marking the current
track; `lilt list` is an alias. `lilt resume` and `lilt toggle` restart the
queue from the beginning when playback has ended.

`lilt shuffle on` reorders the existing queue while keeping the current track.
`lilt shuffle off` disables randomization, restores the original queue order,
and keeps the current track playing; `lilt play --shuffle` randomizes a newly
loaded queue. The TUI `s` key uses the same behavior.

A local `SOURCE` may be absolute, relative to the current directory, or relative
to the music mount — so `lilt play "Brian Eno"` and
`lilt play "Brian Eno/Ambient 1"` both work. Matching falls back to a
case-insensitive search under the mount.

`SOURCE` may also be any URL mpv supports, and `play` accepts multiple sources:

```sh
lilt play 'https://www.youtube.com/watch?v=...'
lilt play 'https://www.youtube.com/playlist?list=...'
lilt add 'https://example.com/stream.mp3' 'https://youtu.be/...'
```

### NTS Radio

`lilt play slow-focus` or `lilt add slow-focus poolside` resolves mixtape
aliases and titles directly. The normal `o` and `a` pickers include an
`[NTS Radio]` entry. It opens a
multi-select list of NTS 1, NTS 2, and every Infinite Mixtape. Select one or
several and they are queued like any other source; switching tracks updates the
title, artist/album, and cover without restarting the panel.

NTS metadata (current show, mixtape title/subtitle) and artwork come from the
public NTS API via `curl`, cached under `~/.cache/lilt`, and shown in the Now
Playing pane and cover. While a live channel is playing, its current show and
artwork refresh every 60 seconds; `lilt status` and `lilt queue` also refresh
stale live metadata. Mixtapes and episode pages are static. Live streams are
URL-only, so the rclone mount is not required. When the library is unmounted,
the pickers still offer NTS plus a `[local library]` entry that mounts on
demand.

Episode pages are resolved through the same API: playing
`https://www.nts.live/shows/blue-screen/episodes/...` shows `NTS — Blue Screen`
and the episode title instead of the uploader name that yt-dlp reports, while
mpv still handles the actual audio stream.

YouTube playback is audio-only (`--ytdl-format=bestaudio/best`), and playlist
URLs expand into separate queue entries. URL-only playback does not require the
rclone mount. Cover art for URL sources is the thumbnail fetched with `yt-dlp`
and cached under `~/.cache/lilt/covers`; playlist entries get their own
thumbnail.

When mpv rejects a URL or yt-dlp fails, `lilt play` exits non-zero and prints
the mpv error plus relevant lines from `mpv.log`.

`browse` returns as soon as you make a selection: it hands the queue to the
detached mpv daemon and exits, so the picker window closes immediately.

### Control panel

`lilt panel` is a ratatui UI with three panes:

- **Now Playing** — artist/album, title, progress, volume/shuffle/repeat.
- **Spectrum** — live bars from the music player’s output (`v` toggles it).
- **Queue** — always visible, with the current track marked.

`Tab` switches focus between Now Playing and Queue; the focused pane's border
is cyan and the other is dim. If the queue is empty when the panel opens, it
automatically runs the add picker first.

The Now Playing pane shows cover art when the terminal is at least 64 columns
wide and 20 rows tall. For URL sources it uses the cached `yt-dlp` thumbnail.
For local files, embedded art is preferred; otherwise a conventional image next
to the track (`cover`, `folder`, `front`, `album`, `artwork`, or `art` with
`.jpg`, `.jpeg`, `.png`, or `.webp`) is used. Kitty, Sixel, and iTerm2 terminals
get full-resolution graphics; other terminals fall back to Unicode halfblocks.
Loading happens on a background thread so tracks on the rclone mount do not
stall the UI. While art is loading or unavailable, the same area shows a small
placeholder. Tracks without a title tag show their file path instead of a
guessed title.

If the mpv daemon exits (`lilt quit`, `lilt unmount`, or `lilt restart`), the
panel closes automatically after a short grace period. `lilt stop` leaves it
open.

**Common keys**

| Key | Action |
| --- | --- |
| `space` | pause/resume, or restart the queue when playback has ended |
| `n` / `p` | next / previous track |
| `x` | stop playback (keep mpv running) |
| `h` / `l` | seek −10s / +10s |
| `H` / `L` | seek −60s / +60s |
| `t` | seek prompt: `N`, `+N`, `-N`, or `N%` |
| `-` / `+` | volume −5 / +5 |
| `s` / `r` / `m` | shuffle / repeat / mute |
| `o` (or `b`) | browse library or NTS → play now (replace queue) |
| `a` | browse library or NTS → add to queue |
| `v` | visualizer on/off |
| `f` | open the playing URL or reveal the local file |
| `Tab` | switch focus (Now Playing ↔ Queue) |
| `/` | focus the queue and filter it |
| `1–9…` then `Enter` or `G` | jump to that queue number (from either pane) |
| `Ctrl-c` | quit the panel (clears an active filter first) |
| `q` / `Q` | detach / quit mpv |

**While Now Playing has focus**

| Key | Action |
| --- | --- |
| `j` / `k` | next / previous track |
| `←` / `→` | seek −10s / +10s |
| `↑` / `↓` | volume +5 / −5 |

**While Queue has focus**

| Key | Action |
| --- | --- |
| `j` / `k` | move selection |
| `PgUp` / `PgDn` | full-page up / down |
| `Ctrl-d` / `Ctrl-u` | half-page down / up |
| `Ctrl-f` / `Ctrl-b` | full-page down / up |
| `↑` / `↓` | move selection |
| `Enter` | switch to the selected track |
| `Esc` | clear the filter, else focus Now Playing |

- The `/` filter is case-insensitive; `Enter` keeps it, `Esc` clears it, `Ctrl-u` clears the query while editing, and `Backspace` deletes a character.
- While entering a queue number, `Backspace` edits it and `Esc` cancels.
- The picker (fzf) temporarily takes over the terminal, then returns to the panel.

### Visualizer

The spectrum pane captures the **mpv stream only** on Linux: it matches the mpv
daemon PID in `pactl list sink-inputs`, then runs
`parec --monitor-stream=<index>`. The daemon runs with
`--audio-client-name=lilt`, and its PID is also resolved through PipeWire’s
client entry when the stream properties omit it. Bars stop when the music stops
and other apps do not affect them. `v` toggles it, and it is hidden below 20
terminal rows.
Set `capture_device` to explicitly record another PulseAudio source instead;
recording a monitor source will make it system-wide again.

On macOS the pane taps the **mpv process** with `audia-tap` instead. mpv keeps
playing to whatever the system output is (AirPods, speakers, …), so there is no
Multi-Output device or routing change, and the bars only react to `lilt`. The
first capture prompts for System Audio Recording; `audia-tap` runs a small
permission agent from its app bundle. If it is not installed, the pane stays
unavailable.

To use the `ffmpeg` + BlackHole fallback instead, set
`capture_device = "BlackHole 2ch"` (or an AVFoundation audio index) and create a
Multi-Output device containing BlackHole and the output you listen on.
`LILT_CAPTURE_DEVICE` overrides `capture_device`; the legacy
`MUSIC_CAPTURE_DEVICE` still works during migration. The fallback captures
whatever is sent to BlackHole, so route mpv through that Multi-Output with
`mpv_extra_args = ["--audio-device=coreaudio/<device-id>"]` (find the id with
`mpv --audio-device=help`).

The UI polls mpv over one persistent socket, reconnects if the daemon restarts,
and restores the terminal on exit.

## Configuration

Configuration lives in `~/.config/lilt/config.toml` (managed by chezmoi in
this setup):

```toml
music_dir = "~/Music"
remote = "sam:/Volumes/NASH_SYNC/music/library"
mpv_extra_args = []
```

`LILT_DIR` and `LILT_REMOTE` environment variables override the file.
`music_dir` falls back to `~/Music` if omitted; `remote` is required.
`mpv_extra_args` is appended to the daemon's mpv command line (useful for
testing with `--ao=null`). During migration, Lilt falls back to
`~/.config/music/config.toml` and the legacy `MUSIC_*` environment variables.
The installer also creates a temporary `music` alias and stops a pre-rename
`music` daemon if one is still running. Fish completions for both names are in
`completions/`.

### Gapless playback

Tracks are streamed over the rclone mount, so the gap between tracks depends on
how fast the next file can be opened. mpv is started with:

- `--gapless-audio=yes` — keep the audio device open across files
- `--prefetch-playlist=yes` — open the next playlist entry before the current one ends
- `--audio-buffer=0.5` — buffer enough audio to cover the network open
- `--demuxer-max-bytes=256MiB --demuxer-readahead-secs=30` — keep the next file ready early
- `--autoload-files=no` — never auto-open sidecars (cover images, subtitles, external audio)

These are intentionally mpv-only: no rclone disk cache, so playback starts
immediately and nothing is written to a cache directory. If a particular album
still has gaps (for example, tracks with different sample rates), the next step
would be a local rclone VFS cache rather than larger demux buffers.

## How it works

- `rclone mount <remote> <music_dir> --read-only --dir-cache-time=10m`
- one detached mpv process:
  `mpv --idle --no-video --no-terminal --input-ipc-server=…/mpv.sock`
- the Rust binary talks to mpv over JSON on the Unix socket; all commands and
  properties are handled in-process (no Python or jq)
- explicit paths/albums are expanded by the wrapper into an m3u using an
  audio-extension allowlist (hidden files and AppleDouble `._*` sidecars are
  skipped); whole-library shuffle lets mpv expand the directory asynchronously
- state lives outside the repo:
  - socket: `${XDG_RUNTIME_DIR:-/tmp}/lilt-mount-$UID/mpv.sock`
  - logs and pid files: `~/.cache/lilt/`

## Desktop integration (Linux/niri)

Example [niri](https://github.com/YaLTeR/niri) binding that focuses an existing
panel window or opens one as a normal tiled window:

```kdl
Mod+M { spawn-sh "~/.config/niri/focus-or-spawn-lilt.sh"; }
```

The helper wraps:

```sh
ghostty --class=com.mitchellh.ghostty.lilt -e "$(command -v lilt)" panel
```

No window rule is needed, so it tiles like any other window.
