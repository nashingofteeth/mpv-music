use image::DynamicImage;
use lofty::file::TaggedFileExt;
use lofty::probe::Probe;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Keep decoded art small enough to encode quickly while still looking sharp
/// on high-DPI terminal graphics protocols.
const MAX_DIMENSION: u32 = 1200;

const FOLDER_NAMES: [&str; 6] = ["cover", "folder", "front", "album", "artwork", "art"];
const IMAGE_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// Load cover art for a track: prefer art embedded in the audio file, then a
/// conventional image next to it.
pub fn load(dir: &Path, track: &Path) -> Option<DynamicImage> {
    let bytes = embedded_picture(track).or_else(|| folder_picture(dir))?;
    let image = image::load_from_memory(&bytes).ok()?;
    Some(limit_size(image))
}

/// Load cover art from an already-downloaded image file.
pub fn load_path(path: &Path) -> Option<DynamicImage> {
    // NTS artwork URLs can be PNG even when the cache filename ends in .jpg.
    // Sniff the bytes instead of relying on the extension.
    let bytes = std::fs::read(path).ok()?;
    image::load_from_memory(&bytes).ok().map(limit_size)
}

/// Fetch a thumbnail for an external URL with yt-dlp, caching it on disk.
pub fn load_url(cache_dir: &Path, url: &str) -> Option<DynamicImage> {
    let url = crate::queue::external_url(url).unwrap_or(url);
    let covers = cache_dir.join("covers");
    std::fs::create_dir_all(&covers).ok()?;
    let hash = url_hash(url);
    if let Some(path) = cached_cover(&covers, &hash) {
        return image::open(path).ok().map(limit_size);
    }
    let base = covers.join(&hash);
    let status = Command::new("yt-dlp")
        .args([
            "--no-playlist",
            "--playlist-items",
            "1",
            "--skip-download",
            "--write-thumbnail",
            "--socket-timeout",
            "10",
            "--retries",
            "1",
            "--output",
        ])
        .arg(format!("{}.%(ext)s", base.display()))
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }

    image::open(cached_cover(&covers, &hash)?)
        .ok()
        .map(limit_size)
}

fn cached_cover(dir: &Path, hash: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_stem().and_then(|stem| stem.to_str()) == Some(hash)
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        IMAGE_EXTENSIONS
                            .iter()
                            .any(|known| known.eq_ignore_ascii_case(extension))
                    })
        })
}

pub(crate) fn url_hash(url: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in url.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn embedded_picture(track: &Path) -> Option<Vec<u8>> {
    let tagged = Probe::open(track).ok()?.read().ok()?;
    tagged.tags().iter().find_map(|tag| {
        tag.pictures()
            .first()
            .map(|picture| picture.data().to_vec())
    })
}

fn folder_picture(dir: &Path) -> Option<Vec<u8>> {
    let path = folder_picture_path(dir)?;
    std::fs::read(path).ok()
}

fn folder_picture_path(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_ascii_lowercase();
            let (stem, extension) = name.rsplit_once('.')?;
            let name_rank = FOLDER_NAMES
                .iter()
                .position(|candidate| *candidate == stem)?;
            let extension_rank = IMAGE_EXTENSIONS
                .iter()
                .position(|candidate| *candidate == extension)?;
            Some((name_rank * IMAGE_EXTENSIONS.len() + extension_rank, path))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, path)| path)
}

fn limit_size(image: DynamicImage) -> DynamicImage {
    if image.width() > MAX_DIMENSION || image.height() > MAX_DIMENSION {
        image.thumbnail(MAX_DIMENSION, MAX_DIMENSION)
    } else {
        image
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_base;

    #[test]
    fn finds_folder_art_case_insensitively() {
        let base = unique_base("cover");
        std::fs::create_dir_all(&base).unwrap();
        let image = image::RgbImage::from_pixel(8, 8, image::Rgb([10, 20, 30]));
        image
            .save(base.join("FRONT.PNG"))
            .expect("write test cover");

        let track = base.join("song.flac");
        let cover = load(&base, &track).expect("folder cover");
        assert_eq!((cover.width(), cover.height()), (8, 8));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn prefers_cover_name_and_jpeg_extension() {
        let base = unique_base("cover-priority");
        std::fs::create_dir_all(&base).unwrap();
        let red = image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]));
        let green = image::RgbImage::from_pixel(4, 4, image::Rgb([0, 255, 0]));
        red.save(base.join("front.jpg")).unwrap();
        green.save(base.join("cover.png")).unwrap();

        let path = folder_picture_path(&base).expect("cover path");
        assert_eq!(path.file_name().unwrap(), "cover.png");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn load_path_sniffs_image_format() {
        let base = unique_base("cover-path-format");
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("cover.jpg");
        let image = image::RgbImage::from_pixel(8, 8, image::Rgb([10, 20, 30]));
        image
            .save_with_format(&path, image::ImageFormat::Png)
            .expect("write png data with jpg name");

        let cover = load_path(&path).expect("load by content");
        assert_eq!((cover.width(), cover.height()), (8, 8));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn uses_cached_url_thumbnail() {
        let base = unique_base("cover-url");
        let cache = base.join("cache");
        let covers = cache.join("covers");
        std::fs::create_dir_all(&covers).unwrap();
        let url = "https://example.com/video";
        let image = image::RgbImage::from_pixel(8, 8, image::Rgb([10, 20, 30]));
        image
            .save(covers.join(format!("{}.png", url_hash(url))))
            .expect("write cached thumbnail");

        let cover = load_url(&cache, url).expect("cached thumbnail");
        assert_eq!((cover.width(), cover.height()), (8, 8));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn uses_cached_thumbnail_for_ytdl_url() {
        let base = unique_base("cover-url-ytdl");
        let cache = base.join("cache");
        let covers = cache.join("covers");
        std::fs::create_dir_all(&covers).unwrap();
        let url = "https://example.com/video";
        let image = image::RgbImage::from_pixel(8, 8, image::Rgb([10, 20, 30]));
        image
            .save(covers.join(format!("{}.png", url_hash(url))))
            .expect("write cached thumbnail");

        let cover = load_url(&cache, &format!("ytdl://{url}")).expect("cached thumbnail");
        assert_eq!((cover.width(), cover.height()), (8, 8));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn url_hash_is_stable_and_distinct() {
        assert_eq!(
            url_hash("https://www.youtube.com/watch?v=abc"),
            url_hash("https://www.youtube.com/watch?v=abc")
        );
        assert_ne!(
            url_hash("https://www.youtube.com/watch?v=abc"),
            url_hash("https://www.youtube.com/watch?v=def")
        );
    }

    #[test]
    fn returns_none_without_art() {
        let base = unique_base("cover-none");
        std::fs::create_dir_all(&base).unwrap();
        assert!(load(&base, &base.join("song.flac")).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
