//! Saving a song as an MP3 or M4A where other apps find it: the shared Music/Sonora folder on
//! a phone, `~/Music/Sonora` on a desktop. A download is different, and stays as the provider
//! sent it in Sonora's own folder.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use gpui::App;
use music::Track;
pub use music::export::Format;
use music::export::Tags;

use crate::toast::{Outcome, Toast, Toasts};
use crate::{Io, Sonora, join};

/// Decodes `track` and saves it as `format`, with a toast while it runs and one when it is
/// done or failed. The fetch, the encoding and the copy all happen off the main thread.
pub fn save_as(track: &Track, format: Format, cx: &mut App) {
    let Some(id) = track.id.clone() else {
        Toasts::show(Outcome::Failed, "toast-song-save-unavailable", cx);
        return;
    };
    let sonora = Sonora::global(cx);
    let (playback, cover) = (sonora.playback.clone(), sonora.cover.clone());
    let name = file_name(track, format);
    let scratch = scratch_dir().join(format!("{:016x}.{}", rand_suffix(&id), format.extension()));
    if let Some(parent) = scratch.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let Some(work) = playback.read(cx).export(&id, format, scratch.clone()) else {
        Toasts::show(Outcome::Failed, "toast-song-save-unavailable", cx);
        return;
    };
    let cover_url = track
        .album_id
        .as_deref()
        .and_then(|album| cover.read(cx).large_for(album))
        .or(track.cover.as_deref())
        .map(str::to_owned);
    let tags = Tags {
        title: track.name.clone(),
        artist: track.artists.clone(),
        album: Some(track.album.clone()),
        cover: None,
    };
    let toast = Toasts::post(
        Toast::new(Outcome::Loading, "toast-song-saving").name(track.name.clone()),
        cx,
    );
    let io = Io::global(cx);
    let title = track.name.clone();
    cx.spawn(async move |cx| {
        let saved = join(io.spawn(async move {
            let encoded = work.await;
            if let Err(error) = encoded {
                std::fs::remove_file(&scratch).ok();
                return Err(error);
            }
            let mut tags = tags;
            if let Some(url) = cover_url {
                match cover_bytes(&url).await {
                    Ok(bytes) => tags.cover = Some(bytes),
                    Err(error) => log::debug!("export: no cover for the tags: {error:#}"),
                }
            }
            tokio::task::spawn_blocking(move || {
                if let Err(error) = music::export::tag(&scratch, &tags) {
                    log::warn!("export: cannot tag the file: {error:#}");
                }
                let placed = place(&scratch, &name, format);
                std::fs::remove_file(&scratch).ok();
                placed
            })
            .await
            .context("the save stopped")?
        }))
        .await;
        cx.update(|cx| {
            let settled = match saved {
                Ok(at) => {
                    log::info!("export: saved {at}");
                    Toast::new(Outcome::Done, "toast-song-saved")
                        .name(title)
                        .detail(at)
                }
                Err(error) => {
                    log::warn!("export: cannot save the song: {error:#}");
                    Toast::new(Outcome::Failed, "toast-song-save-failed")
                        .name(title)
                        .detail(format!("{error:#}"))
                }
            };
            Toasts::settle(toast, settled, cx);
        });
    })
    .detach();
}

/// Where an export is encoded before it is copied out, in Sonora's cache.
fn scratch_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("sonora")
        .join("exports")
}

/// A name for the scratch file that two saves of one song at once do not share.
fn rand_suffix(id: &str) -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write(id.as_bytes());
    hasher.finish()
}

/// `Artist - Title.ext`, with what a file name cannot hold replaced.
fn file_name(track: &Track, format: Format) -> String {
    let stem = match track.artists.trim() {
        "" => track.name.trim().to_owned(),
        artists => format!("{artists} - {}", track.name.trim()),
    };
    let stem: String = stem
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let stem = stem.trim().trim_start_matches('.');
    let stem: String = match stem.is_empty() {
        true => "Song".to_owned(),
        false => stem.chars().take(150).collect(),
    };
    format!("{stem}.{}", format.extension())
}

/// The cover at `url` as JPEG or PNG bytes, which every tag reader shows.
async fn cover_bytes(url: &str) -> Result<Vec<u8>> {
    let bytes = match url.strip_prefix("file://") {
        Some(path) => std::fs::read(path).context("cannot read the cover")?,
        None if url.starts_with('/') => std::fs::read(url).context("cannot read the cover")?,
        None if url.starts_with("http://") || url.starts_with("https://") => reqwest::get(url)
            .await
            .context("cannot request the cover")?
            .error_for_status()
            .context("the cover request was refused")?
            .bytes()
            .await
            .context("cannot read the cover")?
            .to_vec(),
        None => anyhow::bail!("the cover is not a file or a web address"),
    };
    let format = image::guess_format(&bytes).context("cannot tell the cover format")?;
    if matches!(format, image::ImageFormat::Jpeg | image::ImageFormat::Png) {
        return Ok(bytes);
    }
    let decoded = image::load_from_memory_with_format(&bytes, format)
        .context("cannot decode the cover")?
        .into_rgb8();
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&decoded)
        .context("cannot convert the cover")?;
    Ok(jpeg)
}

/// Copies the finished file from `scratch` to where it is kept, and answers where that is.
#[cfg(target_os = "android")]
fn place(scratch: &Path, name: &str, format: Format) -> Result<String> {
    crate::remote::save_to_music(scratch, name, format.mime())
}

/// Copies the finished file from `scratch` to where it is kept, and answers where that is.
#[cfg(not(target_os = "android"))]
fn place(scratch: &Path, name: &str, _format: Format) -> Result<String> {
    let folder = dirs::audio_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Music")))
        .context("there is no music folder")?
        .join("Sonora");
    std::fs::create_dir_all(&folder)
        .with_context(|| format!("cannot create {}", folder.display()))?;
    let path = free_path(&folder, name);
    std::fs::copy(scratch, &path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path.display().to_string())
}

/// `name` in `folder`, numbered past any file already there.
#[cfg(not(target_os = "android"))]
fn free_path(folder: &Path, name: &str) -> PathBuf {
    let path = folder.join(name);
    if !path.exists() {
        return path;
    }
    let (stem, extension) = name.rsplit_once('.').unwrap_or((name, ""));
    (2..)
        .map(|n| folder.join(format!("{stem} ({n}).{extension}")))
        .find(|path| !path.exists())
        .expect("some number is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_name_holds_nothing_a_folder_cannot() {
        let track = Track {
            name: "What? / Why".into(),
            artists: "A:B".into(),
            ..Track::default()
        };
        assert_eq!(file_name(&track, Format::Mp3), "A-B - What- - Why.mp3");
        let bare = Track::default();
        assert_eq!(file_name(&bare, Format::M4a), "Song.m4a");
    }
}
