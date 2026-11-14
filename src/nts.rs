use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::config::Config;
use crate::player;

const LIVE_1: &str = "https://stream-relay-geo.ntslive.net/stream?client=direct";
const LIVE_2: &str = "https://stream-relay-geo.ntslive.net/stream2?client=direct";
const LIVE_API: &str = "https://www.nts.live/api/v2/live";
const MIXTAPES_API: &str = "https://www.nts.live/api/v2/mixtapes";

/// NTS station metadata persisted between the CLI and the control panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Station {
    pub key: String,
    pub title: String,
    pub subtitle: String,
    pub artist: String,
    pub album: String,
    pub stream_url: String,
    pub cover_url: Option<String>,
    pub cover_path: Option<PathBuf>,
    pub live: bool,
}

impl Station {
    fn label(&self) -> String {
        if self.live {
            format!("{} — {}", self.artist, self.title)
        } else {
            format!("{} — {}", self.album, self.title)
        }
    }
}

pub fn play_many(config: &Config, stations: &[Station]) -> Result<()> {
    let stations = if stations.is_empty() {
        vec![live_or_static(1)]
    } else {
        stations.to_vec()
    };
    save_stations(config, &stations);
    let urls: Vec<String> = stations
        .into_iter()
        .map(|station| station.stream_url)
        .collect();
    player::play(config, &urls, false)
}

pub fn add_many(config: &Config, stations: &[Station]) -> Result<()> {
    if stations.is_empty() {
        bail!("nothing to add");
    }
    save_stations(config, stations);
    let inputs: Vec<crate::queue::Input> = stations
        .iter()
        .map(|station| crate::queue::Input::Url(station.stream_url.clone()))
        .collect();
    player::append_inputs(config, &inputs)
}

/// Persist picker metadata without refetching it. Artwork is downloaded lazily
/// by the control panel's cover thread.
fn save_stations(config: &Config, stations: &[Station]) {
    for station in stations {
        let _ = save_station(config, station);
    }
}

/// Resolve a station name, download its artwork, and cache its metadata.
/// Falls back to the cached sidecar when the NTS API is unreachable.
pub fn queue_station(config: &Config, target: &str) -> Option<Station> {
    match resolve(Some(target)) {
        Ok(mut station) => {
            let cover_path = station
                .cover_url
                .as_deref()
                .and_then(|url| download_cover(config, url));
            station.cover_path = cover_path;
            save_station(config, &station).ok()?;
            Some(station)
        }
        Err(_) => cached_match(config, target),
    }
}

fn cached_match(config: &Config, target: &str) -> Option<Station> {
    let wanted = target.trim();
    cached_stations(config).into_iter().find(|station| {
        station.key.eq_ignore_ascii_case(wanted) || station.title.eq_ignore_ascii_case(wanted)
    })
}

pub fn resolve(target: Option<&str>) -> Result<Station> {
    let target = target.unwrap_or("1").trim();
    match target.to_ascii_lowercase().as_str() {
        "" | "1" | "nts1" | "nts 1" | "nts-1" => Ok(live_or_static(1)),
        "2" | "nts2" | "nts 2" | "nts-2" => Ok(live_or_static(2)),
        _ => mixtape(target),
    }
}

pub fn pick(multi: bool) -> Result<Option<Vec<Station>>> {
    let stations = fetch_stations()?;
    let lines: Vec<String> = stations
        .iter()
        .map(|station| format!("{}\t{}", station.key, station.label()))
        .collect();
    let mut args = vec![
        "--delimiter=\t",
        "--with-nth=2",
        "--reverse",
        "--height=100%",
        "--prompt=NTS> ",
        "--header=Tab: multi-select · Enter: play · Esc: cancel",
    ];
    if multi {
        args.push("--multi");
    }
    let picked = crate::browse::fzf(&lines, &args)?;
    let targets: Vec<Station> = picked
        .into_iter()
        .filter_map(|line| {
            let key = line.split('\t').next().filter(|key| !key.is_empty())?;
            stations
                .iter()
                .find(|station| station.key.as_str() == key)
                .cloned()
        })
        .collect();
    if targets.is_empty() {
        Ok(None)
    } else {
        Ok(Some(targets))
    }
}

pub fn cached_stations(config: &Config) -> Vec<Station> {
    let Ok(text) = std::fs::read_to_string(current_path(config)) else {
        return Vec::new();
    };
    if let Ok(stations) = serde_json::from_str::<Vec<Station>>(&text) {
        return stations;
    }
    // Migrate the single-station sidecar written by the first NTS version.
    serde_json::from_str::<Station>(&text)
        .map(|station| vec![station])
        .unwrap_or_default()
}

pub fn current(config: &Config, path: &str) -> Option<Station> {
    find_station(&cached_stations(config), path).cloned()
}

/// Find a cached station or episode by its playback path. NTS episode URLs are
/// matched by show/episode even when query strings or trailing slashes differ.
pub fn find_station<'a>(stations: &'a [Station], path: &str) -> Option<&'a Station> {
    if let Some(station) = stations.iter().find(|station| station.stream_url == path) {
        return Some(station);
    }
    let wanted = episode_parts(path)?;
    stations
        .iter()
        .find(|station| episode_parts(&station.stream_url) == Some(wanted))
}

/// Replace the mpv-reported stream URL with the NTS station identity.
pub fn apply_status(config: &Config, status: &mut crate::mpv::Status) {
    if let Some(station) = current(config, &status.path) {
        status.media_title = station.title;
        status.artist = station.artist;
        status.album = station.album;
        status.has_title_tag = true;
    }
}

pub fn cover(config: &Config, path: &str) -> Option<image::DynamicImage> {
    let mut station = current(config, path).or_else(|| queue_episode(config, path))?;
    // A cached path can be stale after the cache is cleared, so only trust it
    // while the file is still there; otherwise fall back to the artwork URL.
    let cover_path = match station.cover_path.clone().filter(|path| path.is_file()) {
        Some(path) => path,
        None => {
            let url = station.cover_url.as_deref()?;
            let path = download_cover(config, url)?;
            station.cover_path = Some(path.clone());
            save_station(config, &station).ok();
            path
        }
    };
    crate::cover::load_path(&cover_path)
}

fn current_path(config: &Config) -> PathBuf {
    config.cache_dir.join("nts.json")
}

fn save_station(config: &Config, station: &Station) -> Result<()> {
    config.ensure_dirs()?;
    let mut stations = cached_stations(config);
    if let Some(existing) = stations
        .iter_mut()
        .find(|existing| existing.stream_url == station.stream_url)
    {
        let mut updated = station.clone();
        // A picker refresh may not carry artwork yet. Keep a cached cover when
        // the artwork URL is unchanged (or the refresh has no URL at all).
        if updated.cover_path.is_none() {
            if updated.cover_url.is_none() {
                updated.cover_url = existing.cover_url.clone();
                updated.cover_path = existing.cover_path.clone();
            } else if updated.cover_url == existing.cover_url {
                updated.cover_path = existing.cover_path.clone();
            }
        }
        *existing = updated;
    } else {
        stations.push(station.clone());
    }
    let text = serde_json::to_vec_pretty(&stations)?;
    // Replace atomically so the panel never reads a partially written sidecar
    // while a background cover download updates it.
    let path = current_path(config);
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text).context("writing NTS metadata")?;
    std::fs::rename(&temporary, &path).context("replacing NTS metadata")
}

fn download_cover(config: &Config, url: &str) -> Option<PathBuf> {
    let covers = config.cache_dir.join("covers");
    std::fs::create_dir_all(&covers).ok()?;
    let hash = crate::cover::url_hash(url);
    let path = covers.join(format!("nts-{hash}.jpg"));
    if path.is_file() {
        return Some(path);
    }

    let status = Command::new("curl")
        .args(["-fsSL", "--max-time", "15", "-o"])
        .arg(&path)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(path)
}

fn curl_json(url: &str) -> Result<Value> {
    curl_json_with_timeout(url, 15)
}

fn curl_json_with_timeout(url: &str, seconds: u64) -> Result<Value> {
    let max_time = seconds.to_string();
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            max_time.as_str(),
            "-A",
            "lilt/0.1",
            url,
        ])
        .output()
        .context("running curl (needed for NTS metadata)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("NTS request failed: {url}\n{}", stderr.trim());
    }
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parsing NTS response from {url}"))
}

fn live_or_static(channel: u8) -> Station {
    live(channel).unwrap_or_else(|_| static_live(channel))
}

fn live(channel: u8) -> Result<Station> {
    let data = curl_json(LIVE_API)?;
    live_station(channel, &data).with_context(|| format!("NTS {channel} not in the live response"))
}

fn live_station(channel: u8, data: &Value) -> Option<Station> {
    let channel_name = channel.to_string();
    let result = data.get("results")?.as_array()?.iter().find(|result| {
        result.get("channel_name").and_then(Value::as_str) == Some(channel_name.as_str())
    })?;
    let now = result.get("now")?;
    let details = now.get("embeds").and_then(|embeds| embeds.get("details"));
    let title = details
        .and_then(|details| details.get("name"))
        .and_then(Value::as_str)
        .or_else(|| now.get("broadcast_title").and_then(Value::as_str))
        .unwrap_or("NTS");
    let subtitle = details
        .and_then(|details| details.get("description"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let cover_url = details
        .and_then(|details| details.get("media"))
        .and_then(|media| media.get("picture_medium"))
        .and_then(Value::as_str)
        .or_else(|| {
            details
                .and_then(|details| details.get("media"))
                .and_then(|media| media.get("picture_large"))
                .and_then(Value::as_str)
        });

    Some(Station {
        key: channel_name,
        title: decode_html(title),
        subtitle: decode_html(subtitle),
        artist: format!("NTS {channel}"),
        album: "NTS Radio".to_string(),
        stream_url: if channel == 1 { LIVE_1 } else { LIVE_2 }.to_string(),
        cover_url: cover_url.map(str::to_string),
        cover_path: None,
        live: true,
    })
}

fn static_live(channel: u8) -> Station {
    Station {
        key: channel.to_string(),
        title: format!("NTS {channel}"),
        subtitle: String::new(),
        artist: format!("NTS {channel}"),
        album: "NTS Radio".to_string(),
        stream_url: if channel == 1 { LIVE_1 } else { LIVE_2 }.to_string(),
        cover_url: None,
        cover_path: None,
        live: true,
    }
}

fn mixtapes() -> Result<Vec<Station>> {
    let data = curl_json(MIXTAPES_API)?;
    let results = data
        .get("results")
        .and_then(Value::as_array)
        .context("unexpected NTS mixtapes response")?;
    Ok(results.iter().filter_map(mixtape_from_value).collect())
}

fn mixtape(query: &str) -> Result<Station> {
    let stations = mixtapes()?;
    let wanted = query.trim().to_ascii_lowercase();
    if wanted.is_empty() {
        bail!("no NTS mixtape selected");
    }
    stations
        .iter()
        .find(|station| {
            station.key.eq_ignore_ascii_case(&wanted)
                || station.title.to_ascii_lowercase() == wanted
                || station.title.to_ascii_lowercase().replace(' ', "-") == wanted
                || station.title.to_ascii_lowercase().contains(&wanted)
        })
        .cloned()
        .with_context(|| format!("unknown NTS mixtape: {query} (choose it from the NTS picker)"))
}

fn mixtape_from_value(value: &Value) -> Option<Station> {
    let key = value.get("mixtape_alias")?.as_str()?.to_string();
    let title = decode_html(value.get("title")?.as_str()?);
    let subtitle = decode_html(
        value
            .get("subtitle")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let stream_url = value.get("audio_stream_endpoint")?.as_str()?.to_string();
    let cover_url = value
        .get("media")
        .and_then(|media| media.get("picture_medium"))
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("media")
                .and_then(|media| media.get("picture_large"))
                .and_then(Value::as_str)
        });

    Some(Station {
        key,
        title,
        subtitle,
        artist: "NTS".to_string(),
        album: "Infinite Mixtapes".to_string(),
        stream_url,
        cover_url: cover_url.map(str::to_string),
        cover_path: None,
        live: false,
    })
}

/// Whether `input` is an NTS show episode page (not a live/mixtape stream).
pub fn is_episode_url(input: &str) -> bool {
    episode_parts(input).is_some()
}

/// Resolve an NTS episode page through the public API and cache its display
/// metadata. Best effort: on failure playback still proceeds via yt-dlp.
pub fn queue_episode(config: &Config, url: &str) -> Option<Station> {
    let (show, episode) = episode_parts(url)?;
    if let Some(station) = current(config, url) {
        return Some(station);
    }

    let api = format!("https://www.nts.live/api/v2/shows/{show}/episodes/{episode}");
    let data = curl_json_with_timeout(&api, 6).ok()?;
    let show_name = show_name(show).unwrap_or_else(|| humanize_alias(show));
    let station = episode_station(show, episode, &show_name, &data)?;
    save_station(config, &station).ok()?;
    Some(station)
}

fn episode_parts(input: &str) -> Option<(&str, &str)> {
    let rest = input.trim();
    let rest = rest
        .strip_prefix("https://")
        .or_else(|| rest.strip_prefix("http://"))?;
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let rest = rest.split(['?', '#']).next()?;
    let mut parts = rest.split('/').filter(|part| !part.is_empty());
    if parts.next()? != "nts.live" || parts.next()? != "shows" {
        return None;
    }
    let show = parts.next()?;
    if parts.next()? != "episodes" {
        return None;
    }
    let episode = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some((show, episode))
}

fn canonical_episode_url(show: &str, episode: &str) -> String {
    format!("https://www.nts.live/shows/{show}/episodes/{episode}")
}

fn humanize_alias(alias: &str) -> String {
    alias
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn show_name(show: &str) -> Option<String> {
    let api = format!("https://www.nts.live/api/v2/shows/{show}");
    let data = curl_json_with_timeout(&api, 6).ok()?;
    let name = decode_html(data.get("name")?.as_str()?);
    (!name.is_empty()).then_some(name)
}

fn episode_station(show: &str, episode: &str, show_name: &str, data: &Value) -> Option<Station> {
    let title = decode_html(data.get("name")?.as_str()?);
    if title.is_empty() {
        return None;
    }
    let subtitle = decode_html(
        data.get("description")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let media = data.get("media");
    let cover_url = media
        .and_then(|media| media.get("picture_medium"))
        .and_then(Value::as_str)
        .or_else(|| {
            media
                .and_then(|media| media.get("picture_large"))
                .and_then(Value::as_str)
        });

    Some(Station {
        key: episode.to_string(),
        title,
        subtitle,
        artist: "NTS".to_string(),
        album: show_name.trim().to_string(),
        stream_url: canonical_episode_url(show, episode),
        cover_url: cover_url.map(str::to_string),
        cover_path: None,
        live: false,
    })
}

fn fetch_stations() -> Result<Vec<Station>> {
    let mut stations = Vec::new();
    match curl_json(LIVE_API) {
        Ok(data) => {
            for channel in [1, 2] {
                stations.push(live_station(channel, &data).unwrap_or_else(|| static_live(channel)));
            }
        }
        Err(_) => {
            stations.push(static_live(1));
            stations.push(static_live(2));
        }
    }
    stations.extend(mixtapes()?);
    Ok(stations)
}

fn decode_html(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_live_station_metadata() {
        let data = json!({
            "results": [{
                "channel_name": "1",
                "now": {
                    "broadcast_title": "Show &amp; Tell",
                    "embeds": {
                        "details": {
                            "name": "Show &amp; Tell",
                            "description": "A show",
                            "media": {"picture_medium": "https://example.com/show.jpg"}
                        }
                    }
                }
            }]
        });
        let station = live_station(1, &data).expect("live station");
        assert_eq!(station.title, "Show & Tell");
        assert_eq!(station.artist, "NTS 1");
        assert_eq!(station.album, "NTS Radio");
        assert_eq!(station.stream_url, LIVE_1);
        assert_eq!(
            station.cover_url.as_deref(),
            Some("https://example.com/show.jpg")
        );
        assert!(station.live);
    }

    #[test]
    fn parses_mixtape_metadata() {
        let value = json!({
            "mixtape_alias": "slow-focus",
            "title": "Slow Focus",
            "subtitle": "Meditative and beatless",
            "audio_stream_endpoint": "https://example.com/mixtape",
            "media": {"picture_medium": "https://example.com/mixtape.jpg"}
        });
        let station = mixtape_from_value(&value).expect("mixtape");
        assert_eq!(station.title, "Slow Focus");
        assert_eq!(station.artist, "NTS");
        assert_eq!(station.album, "Infinite Mixtapes");
        assert_eq!(station.stream_url, "https://example.com/mixtape");
        assert_eq!(
            station.cover_url.as_deref(),
            Some("https://example.com/mixtape.jpg")
        );
    }

    #[test]
    fn parses_episode_urls() {
        assert_eq!(
            episode_parts(
                "https://www.nts.live/shows/blue-screen/episodes/blue-screen-11th-november-2026"
            ),
            Some(("blue-screen", "blue-screen-11th-november-2026"))
        );
        assert_eq!(
            episode_parts("http://nts.live/shows/a/episodes/b/?utm_source=x#top"),
            Some(("a", "b"))
        );
        assert!(!is_episode_url("https://www.nts.live/shows/blue-screen"));
        assert!(!is_episode_url(
            "https://www.nts.live/shows/a/episodes/b/extra"
        ));
        assert!(!is_episode_url("https://example.com/shows/a/episodes/b"));
        assert!(!is_episode_url("slow-focus"));
    }

    #[test]
    fn parses_episode_metadata() {
        let data = json!({
            "name": "Blue Screen w/ Jim Jarmusch & Jozef Van Wissem ",
            "description": "A show &amp; more",
            "media": {"picture_medium": "https://example.com/episode.jpg"}
        });
        let station = episode_station(
            "blue-screen",
            "blue-screen-11th-november-2026",
            "Blue Screen ",
            &data,
        )
        .expect("episode");

        assert_eq!(
            station.title,
            "Blue Screen w/ Jim Jarmusch & Jozef Van Wissem"
        );
        assert_eq!(station.artist, "NTS");
        assert_eq!(station.album, "Blue Screen");
        assert_eq!(station.subtitle, "A show & more");
        assert_eq!(station.key, "blue-screen-11th-november-2026");
        assert_eq!(
            station.stream_url,
            "https://www.nts.live/shows/blue-screen/episodes/blue-screen-11th-november-2026"
        );
        assert_eq!(
            station.cover_url.as_deref(),
            Some("https://example.com/episode.jpg")
        );
    }

    #[test]
    fn matches_episode_url_variants() {
        let data = json!({"name": "Blue Screen"});
        let station = episode_station(
            "blue-screen",
            "blue-screen-11th-november-2026",
            "Blue Screen",
            &data,
        )
        .expect("episode");
        let stations = vec![station];

        assert_eq!(
            find_station(
                &stations,
                "https://nts.live/shows/blue-screen/episodes/blue-screen-11th-november-2026/?utm_source=copy#top"
            )
            .map(|station| station.title.as_str()),
            Some("Blue Screen")
        );
        assert!(find_station(&stations, "https://example.com/other").is_none());
    }

    #[test]
    fn humanizes_show_aliases() {
        assert_eq!(humanize_alias("blue-screen"), "Blue Screen");
        assert_eq!(humanize_alias("4-to-the-floor"), "4 To The Floor");
    }

    #[test]
    fn decodes_basic_html_entities() {
        assert_eq!(decode_html("A &amp; B &#39;live&#39;"), "A & B 'live'");
    }

    fn station(url: &str, title: &str) -> Station {
        Station {
            key: title.to_ascii_lowercase().replace(' ', "-"),
            title: title.to_string(),
            subtitle: String::new(),
            artist: "NTS".to_string(),
            album: "Infinite Mixtapes".to_string(),
            stream_url: url.to_string(),
            cover_url: None,
            cover_path: None,
            live: false,
        }
    }

    #[test]
    fn caches_stations_by_stream_url() {
        let base = crate::test_support::unique_base("nts-cache");
        let config = crate::test_support::test_config(base.join("Music"), &base);
        let mut first = station("https://a", "A");
        let second = station("https://b", "B");

        save_station(&config, &first).unwrap();
        save_station(&config, &second).unwrap();
        assert_eq!(cached_stations(&config).len(), 2);

        first.title = "A2".to_string();
        save_station(&config, &first).unwrap();
        assert_eq!(cached_stations(&config).len(), 2);
        assert_eq!(current(&config, "https://a").unwrap().title, "A2");
        assert!(current(&config, "https://missing").is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn preserves_cached_cover_when_metadata_refreshes() {
        let base = crate::test_support::unique_base("nts-cover-merge");
        let config = crate::test_support::test_config(base.join("Music"), &base);

        let mut first = station("https://a", "A");
        first.cover_url = Some("https://art".to_string());
        first.cover_path = Some(PathBuf::from("/tmp/art.jpg"));
        save_station(&config, &first).unwrap();

        let mut refreshed = first.clone();
        refreshed.title = "A2".to_string();
        refreshed.cover_path = None;
        save_station(&config, &refreshed).unwrap();
        let cached = current(&config, "https://a").unwrap();
        assert_eq!(cached.title, "A2");
        assert_eq!(
            cached.cover_path.as_deref(),
            Some(std::path::Path::new("/tmp/art.jpg"))
        );

        let mut changed = refreshed.clone();
        changed.title = "A3".to_string();
        changed.cover_url = Some("https://new-art".to_string());
        save_station(&config, &changed).unwrap();
        let cached = current(&config, "https://a").unwrap();
        assert_eq!(cached.title, "A3");
        assert_eq!(cached.cover_url.as_deref(), Some("https://new-art"));
        assert!(cached.cover_path.is_none());

        let mut no_url = changed.clone();
        no_url.title = "A4".to_string();
        no_url.cover_url = None;
        save_station(&config, &no_url).unwrap();
        let cached = current(&config, "https://a").unwrap();
        assert_eq!(cached.title, "A4");
        assert_eq!(cached.cover_url.as_deref(), Some("https://new-art"));
        assert!(cached.cover_path.is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reads_legacy_single_station_cache() {
        let base = crate::test_support::unique_base("nts-cache-legacy");
        let config = crate::test_support::test_config(base.join("Music"), &base);
        config.ensure_dirs().unwrap();
        let legacy = station("https://a", "A");
        std::fs::write(current_path(&config), serde_json::to_vec(&legacy).unwrap()).unwrap();

        assert_eq!(cached_stations(&config).len(), 1);
        assert_eq!(current(&config, "https://a").unwrap().title, "A");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn finds_cached_station_by_name() {
        let base = crate::test_support::unique_base("nts-cache-match");
        let config = crate::test_support::test_config(base.join("Music"), &base);
        let station = station("https://a", "Slow Focus");
        save_station(&config, &station).unwrap();

        assert_eq!(
            cached_match(&config, "slow-focus").map(|station| station.stream_url),
            Some("https://a".to_string())
        );
        assert_eq!(
            cached_match(&config, "  Slow Focus ").map(|station| station.stream_url),
            Some("https://a".to_string())
        );
        assert!(cached_match(&config, "missing").is_none());

        let _ = std::fs::remove_dir_all(&base);
    }
}
