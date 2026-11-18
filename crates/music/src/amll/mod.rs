use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::task::JoinSet;

use crate::lyrics::ttml;
use crate::{Lyrics, LyricsHit, LyricsProvider, LyricsQuery};

const SOURCE: &str = "AMLL";
const GET: &str = "https://api.amll.dev/v1/lyrics/get";
const SEARCH: &str = "https://api.amll.dev/v1/lyrics/search";
const TRUST: u32 = 180;
const CANDIDATES: usize = 3;
const AGENT: &str = concat!(
    "sonora/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/sonorahq/sonora)"
);

/// Word-timed TTML lyrics from the AMLL database (`api.amll.dev`).
pub struct Amll {
    http: reqwest::Client,
}

impl Amll {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    async fn get(&self, params: &[(&str, &str)]) -> Result<Option<SongItem>> {
        let response = self
            .http
            .get(GET)
            .query(params)
            .header("User-Agent", AGENT)
            .send()
            .await
            .context("cannot reach amll")?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            anyhow::bail!("amll answered with status {status}");
        }
        let answer: Envelope<SongItem> = response
            .json()
            .await
            .context("cannot read the amll lyric response")?;
        Ok(answer.data)
    }

    async fn find(&self, query: &LyricsQuery, album: Option<&str>) -> Result<Vec<SongItem>> {
        let mut params = vec![
            ("musicName", query.title.as_str()),
            ("artistName", query.artist.as_str()),
            ("pageSize", "10"),
        ];
        if let Some(album) = album {
            params.push(("albumName", album));
        }

        let response = self
            .http
            .get(SEARCH)
            .query(&params)
            .header("User-Agent", AGENT)
            .send()
            .await
            .context("cannot reach amll")?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        if !status.is_success() {
            anyhow::bail!("amll search answered with status {status}");
        }
        let answer: Envelope<SearchData> = response
            .json()
            .await
            .context("cannot read the amll search response")?;
        Ok(answer.data.map(|data| data.items).unwrap_or_default())
    }
}

impl Default for Amll {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: Option<T>,
}

#[derive(Deserialize)]
struct SearchData {
    #[serde(default)]
    items: Vec<SongItem>,
}

#[derive(Clone, Deserialize)]
struct SongItem {
    id: u64,
    #[serde(default, rename = "musicNames")]
    music_names: Vec<String>,
    #[serde(default, rename = "artistNames")]
    artist_names: Vec<String>,
    #[serde(default, rename = "albumNames")]
    album_names: Vec<String>,
    lyrics: Option<String>,
}

#[async_trait]
impl LyricsProvider for Amll {
    fn name(&self) -> &'static str {
        SOURCE
    }

    async fn search(&self, query: &LyricsQuery) -> Result<Vec<LyricsHit>> {
        if let Some((key, id)) = query
            .id_for("spotify")
            .map(|id| ("spotifyId", id))
            .or_else(|| query.id_for("apple").map(|id| ("appleMusicId", id)))
            && let Some(item) = self.get(&[(key, id)]).await?
            && let Some(hit) = hit(query, &item)
        {
            return Ok(vec![hit]);
        }

        let album = query.album.as_deref().filter(|album| !album.is_empty());
        let mut items = self.find(query, album).await?;
        if items.is_empty() && album.is_some() {
            log::debug!(
                "lyrics: amll found no album match for {}, retrying without the album",
                query.title
            );
            items = self.find(query, None).await?;
        }

        let mut tasks = JoinSet::new();
        for item in shortlist(query, items) {
            let amll = Self {
                http: self.http.clone(),
            };
            let query = query.clone();
            tasks.spawn(async move {
                let id = item.id.to_string();
                let fetched = amll
                    .get(&[("id", id.as_str())])
                    .await
                    .inspect_err(|error| {
                        log::warn!("lyrics: amll did not hand over {}: {error:#}", item.id)
                    })
                    .ok()??;
                hit(&query, &fetched)
            });
        }

        let mut hits = Vec::new();
        while let Some(found) = tasks.join_next().await {
            hits.extend(found.ok().flatten());
        }
        Ok(hits)
    }
}

fn shortlist(query: &LyricsQuery, items: Vec<SongItem>) -> Vec<SongItem> {
    let mut candidates: Vec<_> = items
        .into_iter()
        .filter_map(|item| {
            let preview = metadata_hit(query, &item, Lyrics::plain(String::new()));
            crate::lyrics::matched(query, &preview)
                .then_some((crate::lyrics::score(query, &preview), item))
        })
        .collect();
    candidates.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    candidates.truncate(CANDIDATES);
    candidates.into_iter().map(|(_, item)| item).collect()
}

fn hit(query: &LyricsQuery, item: &SongItem) -> Option<LyricsHit> {
    let raw = item.lyrics.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    let sheet = ttml::parse(raw).ok()?;
    if sheet.lyrics.is_empty() {
        return None;
    }
    let mut hit = metadata_hit(query, item, sheet.lyrics);
    hit.writers = sheet.writers;
    Some(hit)
}

fn metadata_hit(query: &LyricsQuery, item: &SongItem, lyrics: Lyrics) -> LyricsHit {
    let artist = match item.artist_names.is_empty() {
        true => query.artist.clone(),
        false => item.artist_names.join(", "),
    };
    let title = item
        .music_names
        .iter()
        .find(|name| {
            let probe = LyricsHit {
                title: (*name).clone(),
                artist: artist.clone(),
                ..Default::default()
            };
            crate::lyrics::matched(query, &probe)
        })
        .or_else(|| item.music_names.first())
        .cloned()
        .unwrap_or_else(|| query.title.clone());
    let album = query
        .album
        .as_deref()
        .and_then(|wanted| {
            item.album_names
                .iter()
                .find(|name| name.eq_ignore_ascii_case(wanted))
                .cloned()
        })
        .or_else(|| item.album_names.first().cloned());

    LyricsHit {
        source: SOURCE,
        trust: TRUST,
        lyrics,
        instrumental: false,
        title,
        artist,
        album,
        duration: None,
        writers: Vec::new(),
    }
}
