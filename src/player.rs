use anyhow::{Result, bail};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::mount;
use crate::mpv::{self, Mpv, Repeat, Status};
use crate::nts;
use crate::platform;
use crate::queue;

pub fn format_time(value: Option<f64>) -> String {
    match value {
        Some(seconds) if seconds.is_finite() && seconds >= 0.0 => {
            let total = seconds as i64;
            if total >= 3600 {
                format!(
                    "{}:{:02}:{:02}",
                    total / 3600,
                    (total % 3600) / 60,
                    total % 60
                )
            } else {
                format!("{}:{:02}", total / 60, total % 60)
            }
        }
        _ => "--:--".to_string(),
    }
}

pub fn display_name(filename: &str) -> String {
    if let Some((_, rest)) = filename.split_once("://") {
        return rest.to_string();
    }
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(filename);
    let tail = stem.rsplit(" - ").next().unwrap_or(stem);
    let cleaned = tail.trim_start_matches(|character: char| {
        character.is_ascii_digit() || matches!(character, ' ' | '.' | '_' | '-')
    });
    if cleaned.is_empty() {
        stem.to_string()
    } else {
        cleaned.to_string()
    }
}

pub fn repeat_label(repeat: Repeat) -> &'static str {
    match repeat {
        Repeat::Off => "off",
        Repeat::All => "all",
        Repeat::One => "one",
    }
}

pub fn status_line(status: &Status) -> String {
    if status.idle || status.media_title.is_empty() {
        return "mpv: idle (queue empty)".to_string();
    }

    let icon = if status.paused { "||" } else { ">" };
    let mut title = status.media_title.clone();
    if !status.artist.is_empty() {
        title = format!("{} — {}", status.artist, title);
    }
    if !status.album.is_empty() {
        title = format!("{title} [{}]", status.album);
    }

    let queue = if status.playlist_count > 0 && status.playlist_pos >= 0 {
        format!("  ({}/{})", status.playlist_pos + 1, status.playlist_count)
    } else {
        String::new()
    };
    let muted = if status.mute { "  muted" } else { "" };

    format!(
        "{} {}  [{} / {}]  vol {:.0}%  shuffle {}  repeat {}{}{}",
        icon,
        title,
        format_time(status.time_pos),
        format_time(status.duration),
        status.volume.floor(),
        if status.shuffle { "on" } else { "off" },
        repeat_label(status.repeat),
        queue,
        muted
    )
}

pub fn status(config: &Config) -> Result<()> {
    if mount::is_mounted(&config.music_dir) {
        println!("library: mounted at {}", config.music_dir.display());
    } else {
        println!("library: not mounted at {}", config.music_dir.display());
    }

    if !mpv::alive(config) {
        println!("mpv: not running");
        return Ok(());
    }

    let mut client = Mpv::connect(&config.socket)?;
    let mut status = client.status()?;
    nts::apply_status(config, &mut status);
    println!("{}", status_line(&status));
    Ok(())
}

pub fn open(config: &Config) -> Result<()> {
    let mut client = connect(config)?;
    let status = client.status()?;
    if status.path.is_empty() {
        bail!("nothing playing");
    }
    let external = queue::external_url(&status.path).is_some();
    open_path(&status.path)?;
    if external {
        println!("opened {}", status.path);
    } else {
        println!("revealed {}", status.path);
    }
    Ok(())
}

pub fn open_path(path: &str) -> Result<()> {
    if let Some(url) = queue::external_url(path) {
        platform::open_url(url)
    } else {
        let path = Path::new(path);
        if !path.exists() {
            bail!("no such file: {}", path.display());
        }
        platform::reveal_file(path)
    }
}

pub fn queue(config: &Config) -> Result<()> {
    let mut client = match Mpv::connect(&config.socket) {
        Ok(client) => client,
        Err(_) => {
            println!("mpv: not running");
            return Ok(());
        }
    };
    let current = client
        .get("playlist-pos")
        .ok()
        .and_then(|value| value.as_i64())
        .unwrap_or(-1);
    let entries = client.playlist()?;

    if entries.is_empty() {
        println!("queue empty");
        return Ok(());
    }

    let nts = nts::cached_stations(config);
    for (index, entry) in entries.iter().enumerate() {
        let marker = if index as i64 == current { ">" } else { " " };
        let name = if let Some(station) = nts::find_station(&nts, &entry.filename) {
            station.title.clone()
        } else {
            entry
                .title
                .clone()
                .unwrap_or_else(|| display_name(&entry.filename))
        };
        println!("{marker} {:>4}  {name}", index + 1);
    }
    Ok(())
}

pub fn play(config: &Config, inputs: &[String], shuffle: bool) -> Result<()> {
    if inputs.is_empty() {
        load_directory(config, &config.music_dir, true)?;
        if let Ok(mut client) = Mpv::connect(&config.socket) {
            wait_playing(&mut client);
        }
        return status(config);
    }

    let sources = inputs
        .iter()
        .map(|input| resolve_source(config, input))
        .collect::<Result<Vec<_>>>()?;
    let has_url = sources
        .iter()
        .any(|input| matches!(input, queue::Input::Url(_)));
    let log_start = if mpv::alive(config) {
        log_len(config)
    } else {
        0
    };
    let mut client = load_inputs(config, &sources, shuffle)?;

    if has_url {
        if let Err(error) = wait_for_loaded(&mut client, Duration::from_secs(20)) {
            if let Some(detail) = log_error(config, log_start) {
                bail!("{error}\n{detail}");
            }
            return Err(error);
        }
    } else {
        wait_playing(&mut client);
    }

    status(config)
}

/// Ask mpv to expand a directory itself (fast for the whole library, and the
/// scan continues after the caller returns).
pub fn load_directory(config: &Config, path: &std::path::Path, shuffle: bool) -> Result<()> {
    mount::ensure_mount(config)?;
    mpv::ensure_daemon(config)?;

    let mut client = Mpv::connect(&config.socket)?;
    set_bool(&mut client, "shuffle", shuffle)?;
    client.command_values(vec![
        json!("loadfile"),
        json!(path.to_string_lossy()),
        json!("replace"),
    ])?;
    set_bool(&mut client, "pause", false)?;
    Ok(())
}

pub fn load_replace(config: &Config, paths: &[PathBuf], shuffle: bool) -> Result<()> {
    let inputs: Vec<queue::Input> = paths.iter().cloned().map(queue::Input::Local).collect();
    load_inputs(config, &inputs, shuffle).map(|_| ())
}

fn load_inputs(config: &Config, inputs: &[queue::Input], shuffle: bool) -> Result<Mpv> {
    if inputs
        .iter()
        .any(|input| matches!(input, queue::Input::Local(_)))
    {
        mount::ensure_mount(config)?;
    }
    mpv::ensure_daemon(config)?;

    let (playlist, _count) = queue::build_input_playlist(config, inputs)?;
    let result = (|| -> Result<Mpv> {
        let mut client = Mpv::connect(&config.socket)?;
        set_bool(&mut client, "shuffle", shuffle)?;
        client.command_values(vec![
            json!("loadlist"),
            json!(playlist.to_string_lossy()),
            json!("replace"),
        ])?;
        set_bool(&mut client, "pause", false)?;
        Ok(client)
    })();
    let _ = std::fs::remove_file(&playlist);
    result
}

pub fn add(config: &Config, inputs: &[String]) -> Result<()> {
    if inputs.is_empty() {
        bail!("usage: lilt add PATH|URL|NTS...");
    }
    let sources = inputs
        .iter()
        .map(|input| resolve_source(config, input))
        .collect::<Result<Vec<_>>>()?;
    append_inputs(config, &sources)?;
    println!("queued {} source(s)", sources.len());
    Ok(())
}

/// Resolve a local path or URL, falling back to an NTS live/mixtape name.
fn resolve_source(config: &Config, input: &str) -> Result<queue::Input> {
    if nts::is_episode_url(input) {
        // Best effort: use the NTS API for show/episode metadata while still
        // letting mpv/yt-dlp resolve the actual audio stream.
        nts::queue_episode(config, input);
        return Ok(queue::Input::Url(input.to_string()));
    }
    match queue::resolve_input(config, input) {
        Ok(source) => Ok(source),
        Err(local_error) => {
            if looks_like_nts(input)
                && let Some(station) = nts::queue_station(config, input)
            {
                Ok(queue::Input::Url(station.stream_url))
            } else {
                Err(local_error)
            }
        }
    }
}

/// NTS names have no path separators or extensions, so path typos do not
/// trigger a network lookup.
fn looks_like_nts(input: &str) -> bool {
    !input.is_empty()
        && !input.contains('/')
        && !input.contains('\\')
        && !input.contains('.')
        && !input.starts_with('~')
}

/// Append already-resolved paths to the queue (used by `add` and the TUI).
pub fn append_paths(config: &Config, paths: &[PathBuf]) -> Result<()> {
    let inputs: Vec<queue::Input> = paths.iter().cloned().map(queue::Input::Local).collect();
    append_inputs(config, &inputs)
}

pub(crate) fn append_inputs(config: &Config, inputs: &[queue::Input]) -> Result<()> {
    if inputs.is_empty() {
        bail!("nothing to add");
    }
    if inputs
        .iter()
        .any(|input| matches!(input, queue::Input::Local(_)))
    {
        mount::ensure_mount(config)?;
    }
    mpv::ensure_daemon(config)?;

    let (playlist, _count) = queue::build_input_playlist(config, inputs)?;
    let result = (|| -> Result<()> {
        let mut client = Mpv::connect(&config.socket)?;
        let status = client.status()?;
        let start_index = status.playlist_count;
        let was_idle = status.idle;
        client.command_values(vec![
            json!("loadlist"),
            json!(playlist.to_string_lossy()),
            json!("append"),
        ])?;
        // Appending to an idle player fills the queue but does not start it;
        // jump to the first newly appended entry in that case. Adding to an
        // already playing (possibly paused) queue must not change playback.
        if was_idle {
            client.command_values(vec![json!("playlist-play-index"), json!(start_index)])?;
            set_bool(&mut client, "pause", false)?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&playlist);
    result
}

fn wait_for_loaded(client: &mut Mpv, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        let status = client.status().ok();
        for event in client.drain_events() {
            let name = event
                .get("event")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            if name == "file-loaded" {
                return Ok(());
            }
            if name == "end-file"
                && event.get("reason").and_then(|value| value.as_str()) == Some("error")
            {
                let detail = event
                    .get("file_error")
                    .and_then(|value| value.as_str())
                    .unwrap_or("playback failed");
                bail!("mpv: {detail}");
            }
        }

        if let Some(status) = status
            && !status.idle
            && status.duration.is_some()
        {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            bail!("timed out waiting for mpv to load source (see lilt logs)");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn log_len(config: &Config) -> u64 {
    std::fs::metadata(&config.mpv_log)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn log_error(config: &Config, start: u64) -> Option<String> {
    let bytes = std::fs::read(&config.mpv_log).ok()?;
    let start = usize::try_from(start).ok()?.min(bytes.len());
    let text = String::from_utf8_lossy(&bytes[start..]);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && (line.contains("ERROR")
                    || line.to_ascii_lowercase().contains("error")
                    || line.to_ascii_lowercase().contains("failed"))
        })
        .collect();
    let tail = lines[lines.len().saturating_sub(5)..].join("\n");
    (!tail.is_empty()).then(|| format!("mpv log:\n{tail}"))
}

fn wait_playing(client: &mut Mpv) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if let Ok(status) = client.status()
            && !status.idle
            && status.time_pos.is_some()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn connect(config: &Config) -> Result<Mpv> {
    if !mpv::alive(config) {
        bail!("mpv is not running");
    }
    Mpv::connect(&config.socket)
}

fn set_bool(client: &mut Mpv, name: &str, value: bool) -> Result<()> {
    client.command_values(vec![json!("set_property"), json!(name), json!(value)])?;
    Ok(())
}

pub fn toggle(config: &Config) -> Result<()> {
    let mut client = connect(config)?;
    toggle_client(&mut client)
}

pub fn toggle_client(client: &mut Mpv) -> Result<()> {
    if is_idle(client)? {
        resume_client(client)
    } else {
        client.command(&["cycle", "pause"])
    }
}

pub fn pause(config: &Config) -> Result<()> {
    let mut client = connect(config)?;
    set_bool(&mut client, "pause", true)
}

pub fn resume(config: &Config) -> Result<()> {
    let mut client = connect(config)?;
    resume_client(&mut client)
}

pub fn resume_client(client: &mut Mpv) -> Result<()> {
    if is_idle(client)? {
        let count = queue_count(client)?;
        if count <= 0 {
            bail!("queue empty");
        }
        client.command_values(vec![json!("playlist-play-index"), json!(0)])?;
    }
    set_bool(client, "pause", false)
}

fn is_idle(client: &mut Mpv) -> Result<bool> {
    if client
        .try_get("idle-active")?
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        return Ok(true);
    }
    // At the end of a playlist mpv can keep idle-active false for a moment
    // while it unloads the file. A missing path means there is nothing loaded.
    let has_path = client
        .try_get("path")?
        .and_then(|value| value.as_str().map(|path| !path.is_empty()))
        .unwrap_or(false);
    Ok(!has_path)
}

pub fn next(config: &Config) -> Result<()> {
    let mut client = connect(config)?;
    if client.command(&["playlist-next"]).is_err() {
        let _ = client.command_values(vec![json!("playlist-play-index"), json!(0)]);
    }
    Ok(())
}

pub fn prev(config: &Config) -> Result<()> {
    let mut client = connect(config)?;
    if client.command(&["playlist-prev"]).is_err() {
        let count = client
            .get("playlist-count")
            .ok()
            .and_then(|value| value.as_i64())
            .unwrap_or(0);
        if count > 0 {
            let _ = client.command_values(vec![json!("playlist-play-index"), json!(count - 1)]);
        }
    }
    Ok(())
}

pub fn stop(config: &Config) -> Result<()> {
    connect(config)?.command(&["stop"])
}

pub fn mute(config: &Config, mode: Option<&str>) -> Result<()> {
    let mut client = connect(config)?;
    match mode.unwrap_or("toggle") {
        "on" | "yes" | "true" => set_bool(&mut client, "mute", true),
        "off" | "no" | "false" => set_bool(&mut client, "mute", false),
        "toggle" => client.command(&["cycle", "mute"]),
        other => bail!("usage: lilt mute [on|off|toggle] (got {other})"),
    }
}

pub fn jump(config: &Config, number: usize) -> Result<()> {
    let mut client = connect(config)?;
    jump_index(&mut client, number)
}

fn queue_count(client: &mut Mpv) -> Result<i64> {
    Ok(client
        .get("playlist-count")
        .ok()
        .and_then(|value| value.as_i64())
        .unwrap_or(0))
}

fn jump_index(client: &mut Mpv, number: usize) -> Result<()> {
    let count = queue_count(client)?;
    if number == 0 || number as i64 > count {
        bail!("no track {number} (queue has {count})");
    }
    client.command_values(vec![
        json!("playlist-play-index"),
        json!((number - 1) as i64),
    ])?;
    set_bool(client, "pause", false)
}

pub fn volume(config: &Config, value: &str) -> Result<()> {
    let command = if let Some(value) = value.strip_prefix('+') {
        let amount = parse_number(value, "volume")?;
        vec![json!("add"), json!("volume"), json!(amount)]
    } else if value.starts_with('-') {
        let amount = parse_number(value, "volume")?;
        vec![json!("add"), json!("volume"), json!(amount)]
    } else {
        let amount = parse_number(value, "volume")?;
        vec![json!("set_property"), json!("volume"), json!(amount)]
    };

    let mut client = connect(config)?;
    client.command_values(command)?;
    Ok(())
}

pub fn seek(config: &Config, value: &str) -> Result<()> {
    let command = seek_command(value)?;
    let mut client = connect(config)?;
    client.command_values(command)?;
    Ok(())
}

pub fn seek_client(client: &mut Mpv, value: &str) -> Result<()> {
    client.command_values(seek_command(value)?)?;
    Ok(())
}

fn seek_command(value: &str) -> Result<Vec<serde_json::Value>> {
    Ok(if let Some(percent) = value.strip_suffix('%') {
        let amount = parse_number(percent, "seek")?;
        vec![json!("seek"), json!(amount), json!("absolute-percent")]
    } else if let Some(relative) = value.strip_prefix('+') {
        let amount = parse_number(relative, "seek")?;
        vec![json!("seek"), json!(amount), json!("relative")]
    } else if value.starts_with('-') {
        let amount = parse_number(value, "seek")?;
        vec![json!("seek"), json!(amount), json!("relative")]
    } else {
        let amount = parse_number(value, "seek")?;
        vec![json!("seek"), json!(amount), json!("absolute")]
    })
}

pub fn shuffle(config: &Config, mode: Option<&str>) -> Result<()> {
    let mut client = connect(config)?;
    match mode.unwrap_or("toggle") {
        "on" | "yes" | "true" => set_shuffle_client(&mut client, true),
        "off" | "no" | "false" => set_shuffle_client(&mut client, false),
        "toggle" => toggle_shuffle_client(&mut client),
        other => bail!("usage: lilt shuffle [on|off|toggle] (got {other})"),
    }
}

pub fn toggle_shuffle_client(client: &mut Mpv) -> Result<()> {
    let enabled = client
        .try_get("shuffle")?
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    set_shuffle_client(client, !enabled)
}

pub fn set_shuffle_client(client: &mut Mpv, enabled: bool) -> Result<()> {
    set_bool(client, "shuffle", enabled)?;
    if enabled {
        client.command(&["playlist-shuffle"])?;
    } else {
        client.command(&["playlist-unshuffle"])?;
    }
    Ok(())
}

pub fn repeat(config: &Config, mode: Option<&str>) -> Result<()> {
    let mut client = connect(config)?;
    let repeat = match mode {
        Some("off" | "no") => Repeat::Off,
        Some("all") => Repeat::All,
        Some("one") => Repeat::One,
        None => bail!("usage: lilt repeat [off|all|one]"),
        Some(other) => bail!("usage: lilt repeat [off|all|one] (got {other})"),
    };
    set_repeat(&mut client, repeat)
}

pub fn set_repeat(client: &mut Mpv, repeat: Repeat) -> Result<()> {
    match repeat {
        Repeat::Off => {
            set_bool(client, "loop-file", false)?;
            set_bool(client, "loop-playlist", false)?;
        }
        Repeat::All => {
            set_bool(client, "loop-file", false)?;
            set_bool(client, "loop-playlist", true)?;
        }
        Repeat::One => {
            set_bool(client, "loop-playlist", false)?;
            set_bool(client, "loop-file", true)?;
        }
    }
    Ok(())
}

pub fn quit(config: &Config) -> Result<()> {
    mpv::quit(config, false)
}

pub fn restart(config: &Config) -> Result<()> {
    mount::unmount(config)
}

fn parse_number(value: &str, what: &str) -> Result<f64> {
    let parsed = value
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("invalid {what} value: {value}"))?;
    if !parsed.is_finite() {
        bail!("invalid {what} value: {value}");
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bare_names_trigger_nts_lookup() {
        assert!(looks_like_nts("slow-focus"));
        assert!(looks_like_nts("Poolside"));
        assert!(looks_like_nts("NTS 1"));
        assert!(!looks_like_nts("/tmp/song.flac"));
        assert!(!looks_like_nts("Artist/Album"));
        assert!(!looks_like_nts("song.flac"));
        assert!(!looks_like_nts("~"));
    }

    #[test]
    fn formats_times() {
        assert_eq!(format_time(None), "--:--");
        assert_eq!(format_time(Some(5.9)), "0:05");
        assert_eq!(format_time(Some(65.0)), "1:05");
        assert_eq!(format_time(Some(3725.0)), "1:02:05");
    }

    #[test]
    fn builds_seek_commands() {
        assert_eq!(
            seek_command("90").unwrap(),
            vec![json!("seek"), json!(90.0), json!("absolute")]
        );
        assert_eq!(
            seek_command("+30").unwrap(),
            vec![json!("seek"), json!(30.0), json!("relative")]
        );
        assert_eq!(
            seek_command("-15").unwrap(),
            vec![json!("seek"), json!(-15.0), json!("relative")]
        );
        assert_eq!(
            seek_command("50%").unwrap(),
            vec![json!("seek"), json!(50.0), json!("absolute-percent")]
        );
        assert_eq!(
            seek_command("+7.5").unwrap(),
            vec![json!("seek"), json!(7.5), json!("relative")]
        );
        assert!(seek_command("nope").is_err());
        assert!(seek_command("inf").is_err());
    }

    #[test]
    fn labels_repeat_modes() {
        assert_eq!(repeat_label(Repeat::Off), "off");
        assert_eq!(repeat_label(Repeat::All), "all");
        assert_eq!(repeat_label(Repeat::One), "one");
    }
}

#[cfg(test)]
mod fuzz_tests {
    use super::*;
    use crate::test_support::{Rng, cases};

    #[test]
    fn fuzz_parse_number_and_format_time_never_panic() {
        let mut rng = Rng::new(77);
        for _ in 0..cases(5_000) {
            let text = rng.string(40);
            let _ = parse_number(&text, "volume");
            let value = rng.f64_bits();
            let _ = format_time(if rng.boolean() { None } else { Some(value) });
        }

        for text in ["NaN", "inf", "-inf", "1e999", "-1e999", ""] {
            assert!(
                parse_number(text, "volume").is_err(),
                "{text:?} should be rejected"
            );
        }
    }
}

#[cfg(test)]
mod status_fuzz_tests {
    use super::*;
    use crate::mpv::Status;
    use crate::test_support::{Rng, cases};

    #[test]
    fn fuzz_status_line_never_panics() {
        let mut rng = Rng::new(0x123);
        for _ in 0..cases(5_000) {
            let status = Status {
                idle: rng.boolean(),
                media_title: rng.string(80),
                artist: rng.string(40),
                album: rng.string(40),
                path: rng.string(80),
                has_title_tag: rng.boolean(),
                time_pos: if rng.boolean() {
                    Some(rng.f64_bits())
                } else {
                    None
                },
                duration: if rng.boolean() {
                    Some(rng.f64_bits())
                } else {
                    None
                },
                paused: rng.boolean(),
                volume: rng.f64_bits(),
                shuffle: rng.boolean(),
                repeat: match rng.below(3) {
                    0 => Repeat::Off,
                    1 => Repeat::All,
                    _ => Repeat::One,
                },
                playlist_pos: rng.below(100) as i64 - 1,
                playlist_count: rng.below(100) as i64,
                paused_for_cache: rng.boolean(),
                mute: rng.boolean(),
            };
            let _ = status_line(&status);
        }
    }
}
