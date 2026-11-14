use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::platform;

pub struct Mpv {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
    events: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub idle: bool,
    pub media_title: String,
    pub artist: String,
    pub album: String,
    pub path: String,
    pub has_title_tag: bool,
    pub time_pos: Option<f64>,
    pub duration: Option<f64>,
    pub paused: bool,
    pub volume: f64,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub playlist_pos: i64,
    pub playlist_count: i64,
    pub paused_for_cache: bool,
    pub mute: bool,
}

#[derive(Debug, Clone)]
pub struct PlaylistEntry {
    pub filename: String,
    pub title: Option<String>,
}

impl Mpv {
    pub fn connect(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("connecting to mpv socket {}", socket.display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            writer: stream,
            reader,
            next_id: 0,
            events: Vec::new(),
        })
    }

    pub fn request(&mut self, command: Vec<Value>) -> Result<Option<Value>> {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({"command": command, "request_id": id});
        writeln!(self.writer, "{request}")?;
        self.writer.flush()?;

        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                bail!("mpv closed the connection");
            }
            let reply: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if reply.get("event").is_some() {
                if self.events.len() >= 256 {
                    self.events.remove(0);
                }
                self.events.push(reply);
                continue;
            }
            if reply.get("request_id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            let error = reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("success");
            if error != "success" {
                bail!("mpv error: {error}");
            }
            return Ok(reply.get("data").cloned());
        }
    }

    pub fn drain_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.events)
    }

    pub fn command(&mut self, command: &[&str]) -> Result<()> {
        self.request(command.iter().map(|part| json!(part)).collect())?;
        Ok(())
    }

    pub fn command_values(&mut self, command: Vec<Value>) -> Result<Option<Value>> {
        self.request(command)
    }

    pub fn get(&mut self, name: &str) -> Result<Value> {
        Ok(self
            .request(vec![json!("get_property"), json!(name)])?
            .unwrap_or(Value::Null))
    }

    /// Like get(), but returns None when mpv says the property is unavailable
    /// (normal while idle) instead of treating it as a hard error.
    pub fn try_get(&mut self, name: &str) -> Result<Option<Value>> {
        match self.request(vec![json!("get_property"), json!(name)]) {
            Ok(value) => Ok(value),
            Err(err) => {
                let text = err.to_string();
                if text.contains("property unavailable") || text.contains("property not found") {
                    Ok(None)
                } else {
                    Err(err)
                }
            }
        }
    }

    pub fn status(&mut self) -> Result<Status> {
        let idle = self
            .try_get("idle-active")?
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let media_title = self
            .try_get("media-title")?
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let mut artist = String::new();
        let mut album_artist = String::new();
        let mut album = String::new();
        let mut uploader = String::new();
        let mut has_title_tag = false;
        if let Some(Value::Object(metadata)) = self.try_get("metadata")? {
            for (key, value) in metadata {
                let Some(text) = value.as_str() else { continue };
                match key.to_ascii_lowercase().as_str() {
                    "title" => has_title_tag |= !text.trim().is_empty(),
                    "artist" => artist = text.to_string(),
                    "album_artist" | "albumartist" => album_artist = text.to_string(),
                    "album" => album = text.to_string(),
                    "uploader" | "channel" => uploader = text.to_string(),
                    _ => {}
                }
            }
        }
        let mut artist = display_artist(&artist, &album_artist);
        if artist.is_empty() {
            artist = uploader.trim().to_string();
        }
        let path = self
            .try_get("path")?
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default();
        let time_pos = self.try_get("time-pos")?.and_then(|v| v.as_f64());
        let duration = self.try_get("duration")?.and_then(|v| v.as_f64());
        let paused = self
            .try_get("pause")?
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let volume = self
            .try_get("volume")?
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let shuffle = self
            .try_get("shuffle")?
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let loop_file = self
            .try_get("loop-file")?
            .map(|v| truthy(&v))
            .unwrap_or(false);
        let loop_playlist = self
            .try_get("loop-playlist")?
            .map(|v| truthy(&v))
            .unwrap_or(false);
        let repeat = if loop_file {
            Repeat::One
        } else if loop_playlist {
            Repeat::All
        } else {
            Repeat::Off
        };
        let playlist_pos = self
            .try_get("playlist-pos")?
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        let playlist_count = self
            .try_get("playlist-count")?
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let paused_for_cache = self
            .try_get("paused-for-cache")?
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mute = self
            .try_get("mute")?
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(Status {
            idle,
            media_title,
            artist,
            album,
            path,
            has_title_tag,
            time_pos,
            duration,
            paused,
            volume,
            shuffle,
            repeat,
            playlist_pos,
            playlist_count,
            paused_for_cache,
            mute,
        })
    }

    pub fn playlist(&mut self) -> Result<Vec<PlaylistEntry>> {
        let value = self.get("playlist")?;
        let mut entries = Vec::new();
        if let Some(items) = value.as_array() {
            for item in items {
                let filename = item
                    .get("filename")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let title = item
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                entries.push(PlaylistEntry { filename, title });
            }
        }
        Ok(entries)
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_i64().unwrap_or(0) != 0,
        Value::String(value) => !matches!(value.as_str(), "no" | "false" | "0" | ""),
        _ => false,
    }
}

/// Collapse duplicate `/`-separated artist names. Some rips repeat the same
/// performer several times; if that happens and the album artist is available,
/// prefer the cleaner album artist.
fn display_artist(artist: &str, album_artist: &str) -> String {
    use std::collections::HashSet;

    let parts: Vec<&str> = artist
        .split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return album_artist.trim().to_string();
    }

    let mut seen = HashSet::new();
    let unique: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|part| seen.insert(part.to_ascii_lowercase()))
        .collect();
    let had_duplicates = unique.len() < parts.len();

    if had_duplicates && !album_artist.trim().is_empty() {
        album_artist.trim().to_string()
    } else if unique.len() > 1 {
        unique.join(" / ")
    } else {
        artist.trim().to_string()
    }
}

pub const AUDIO_CLIENT_NAME: &str = "lilt";

pub fn args(config: &Config) -> Vec<String> {
    let mut args = vec![
        "--idle=yes".to_string(),
        "--no-video".to_string(),
        "--no-terminal".to_string(),
        format!("--log-file={}", config.mpv_log.display()),
        format!("--input-ipc-server={}", config.socket.display()),
        "--directory-mode=recursive".to_string(),
        "--directory-filter-types=audio".to_string(),
        "--gapless-audio=yes".to_string(),
        "--prefetch-playlist=yes".to_string(),
        "--audio-buffer=0.5".to_string(),
        "--autoload-files=no".to_string(),
        "--audio-display=no".to_string(),
        "--ytdl-format=bestaudio/best".to_string(),
        format!("--audio-client-name={AUDIO_CLIENT_NAME}"),
        "--no-resume-playback".to_string(),
        "--save-position-on-quit=no".to_string(),
        "--cache=yes".to_string(),
        "--demuxer-max-bytes=256MiB".to_string(),
        "--demuxer-readahead-secs=30".to_string(),
    ];
    args.extend(config.mpv_extra_args.iter().cloned());
    args
}

pub fn alive(config: &Config) -> bool {
    match Mpv::connect(&config.socket) {
        Ok(mut client) => client.get("idle-active").is_ok(),
        Err(_) => false,
    }
}

/// PID of the running mpv daemon, if the pid file still points at our process.
pub fn daemon_pid(config: &Config) -> Option<u32> {
    let pid = read_pid(&config.mpv_pid)?;
    pid_is_ours(pid, &config.socket).then_some(pid)
}

pub fn ensure_daemon(config: &Config) -> Result<()> {
    if alive(config) {
        return Ok(());
    }

    // A single slow IPC reply can look like a dead daemon. If the pid file
    // still points at our process, give it one more chance before killing it
    // and losing playback.
    if daemon_pid(config).is_some() {
        std::thread::sleep(Duration::from_millis(200));
        if alive(config) {
            return Ok(());
        }
    }

    config.ensure_dirs()?;
    let _ = std::fs::remove_file(&config.socket);

    // Clean up a wedged instance that still owns our socket path.
    let pattern = format!("input-ipc-server={}", config.socket.display());
    let _ = Command::new("pkill")
        .args(["-f", &pattern])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let mut command = Command::new("mpv");
    command.args(args(config));
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    // New session: survive the terminal that launched us.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn().context("starting mpv")?;
    let pid = child.id();
    std::fs::write(&config.mpv_pid, pid.to_string())
        .with_context(|| format!("writing {}", config.mpv_pid.display()))?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if alive(config) {
            return Ok(());
        }
        if !pid_alive(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    bail!("mpv failed to start (see {})", config.mpv_log.display())
}

pub fn quit(config: &Config, quiet: bool) -> Result<()> {
    let pid = read_pid(&config.mpv_pid);
    let mut was_alive = false;

    if let Some(pid) = pid
        && pid_is_ours(pid, &config.socket)
    {
        was_alive = true;
    }

    if alive(config) {
        was_alive = true;
        if let Ok(mut client) = Mpv::connect(&config.socket) {
            // mpv usually closes the socket before replying to quit.
            let _ = client.command(&["quit"]);
        }
    }

    if let Some(pid) = pid {
        if pid_is_ours(pid, &config.socket) {
            wait_for_exit(pid, Duration::from_secs(5));
            if pid_alive(pid) {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
                wait_for_exit(pid, Duration::from_millis(500));
            }
            if pid_alive(pid) {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
    } else {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) && alive(config) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    let _ = std::fs::remove_file(&config.socket);
    let _ = std::fs::remove_file(&config.mpv_pid);

    if !quiet {
        if was_alive {
            println!("mpv stopped");
        } else {
            println!("mpv is not running");
        }
    }
    Ok(())
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

fn pid_alive(pid: u32) -> bool {
    platform::pid_alive(pid)
}

/// Guard against a recycled PID: only signal it if its command line is our mpv.
fn pid_is_ours(pid: u32, socket: &Path) -> bool {
    platform::process_command(pid)
        .map(|text| text.contains(&format!("input-ipc-server={}", socket.display())))
        .unwrap_or(false)
}

fn wait_for_exit(pid: u32, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn open_log_for_append(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

#[cfg(test)]
mod fuzz_tests {
    use super::*;
    use crate::test_support::{Rng, cases, unique_base};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    #[test]
    fn fuzz_truthy_never_panics() {
        let mut rng = Rng::new(99);
        for _ in 0..cases(5_000) {
            let value = match rng.below(7) {
                0 => Value::Null,
                1 => Value::Bool(rng.boolean()),
                2 => json!(rng.next_u64() as i64),
                3 => json!(rng.next_u64() as f64),
                4 => Value::String(rng.string(20)),
                5 => Value::Array(vec![Value::Bool(true), Value::String(rng.string(5))]),
                _ => {
                    let mut object = serde_json::Map::new();
                    object.insert(rng.string(8), Value::String(rng.string(8)));
                    Value::Object(object)
                }
            };
            let _ = truthy(&value);
        }
    }

    #[test]
    fn fuzz_ipc_random_replies_never_panic() {
        let mut rng = Rng::new(1234);
        let iterations = cases(100).min(300);

        for case in 0..iterations {
            let base = unique_base(&format!("ipc-{case}"));
            let path = base.join("mpv.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let payload = if rng.boolean() {
                rng.bytes(200)
            } else {
                rng.string(200).into_bytes()
            };

            let handle = thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut request = [0u8; 4096];
                    let _ = stream.read(&mut request);
                    let _ = stream.write_all(&payload);
                    if !payload.ends_with(b"\n") {
                        let _ = stream.write_all(b"\n");
                    }
                }
            });

            let mut client = Mpv::connect(&path).unwrap();
            let _ = client.request(vec![json!("get_property"), json!("x")]);
            handle.join().unwrap();
            let _ = std::fs::remove_dir_all(&base);
        }
    }
}

#[cfg(test)]
mod status_fuzz_tests {
    use super::*;
    use crate::test_support::{Rng, cases, unique_base};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn random_value(rng: &mut Rng) -> Value {
        match rng.below(8) {
            0 => Value::Null,
            1 => Value::Bool(rng.boolean()),
            2 => json!(rng.next_u64() as i64),
            3 => json!(rng.next_u64() as f64),
            4 => Value::String(rng.string(20)),
            5 => Value::Array(vec![Value::Bool(true), Value::String(rng.string(5))]),
            6 => {
                let mut object = serde_json::Map::new();
                object.insert("Artist".to_string(), Value::String(rng.string(10)));
                object.insert("Album".to_string(), Value::String(rng.string(10)));
                Value::Object(object)
            }
            _ => {
                let mut object = serde_json::Map::new();
                object.insert(rng.string(8), random_value(rng));
                Value::Object(object)
            }
        }
    }

    #[test]
    fn buffers_playback_events() {
        let base = unique_base("mpv-events");
        let path = base.join("mpv.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let id = request.get("request_id").and_then(Value::as_u64).unwrap();
            let _ = writeln!(
                writer,
                "{}",
                json!({"event": "end-file", "reason": "error", "file_error": "boom"})
            );
            let _ = writeln!(
                writer,
                "{}",
                json!({"request_id": id, "error": "success", "data": true})
            );
        });

        let mut client = Mpv::connect(&path).unwrap();
        assert_eq!(
            client
                .request(vec![json!("get_property"), json!("x")])
                .unwrap(),
            Some(Value::Bool(true))
        );
        let events = client.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["file_error"], "boom");
        assert!(client.drain_events().is_empty());
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn fuzz_status_parsing_random_properties_never_panics() {
        let mut rng = Rng::new(0x5EED);
        let iterations = cases(100).min(300);

        for case in 0..iterations {
            let base = unique_base(&format!("status-{case}"));
            let path = base.join("mpv.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let seed = rng.next_u64();

            let handle = thread::spawn(move || {
                let mut local = Rng::new(seed);
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let Ok(request) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let Some(id) = request.get("request_id").and_then(Value::as_u64) else {
                        continue;
                    };
                    let reply = json!({
                        "request_id": id,
                        "error": "success",
                        "data": random_value(&mut local),
                    });
                    let _ = writeln!(writer, "{reply}");
                }
            });

            let mut client = Mpv::connect(&path).unwrap();
            let _ = client.status();
            drop(client);
            handle.join().unwrap();
            let _ = std::fs::remove_dir_all(&base);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_artist_parts_prefer_album_artist() {
        assert_eq!(
            display_artist(
                "Los Angeles Philharmonic/Dmitriy Lipay/Dmitriy Lipay/Alexander Lipay/Alexander Lipay",
                "Los Angeles Philharmonic, Gustavo Dudamel"
            ),
            "Los Angeles Philharmonic, Gustavo Dudamel"
        );
    }

    #[test]
    fn duplicate_parts_are_collapsed_without_album_artist() {
        assert_eq!(display_artist("A/B/B", ""), "A / B");
    }

    #[test]
    fn simple_artist_is_unchanged() {
        assert_eq!(display_artist("A", "Album Artist"), "A");
        assert_eq!(display_artist("A/B", ""), "A / B");
        assert_eq!(display_artist("", "Album Artist"), "Album Artist");
    }
}
