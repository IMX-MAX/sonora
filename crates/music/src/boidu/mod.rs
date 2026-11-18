use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::lyrics::ttml;
use crate::{LyricsHit, LyricsProvider, LyricsQuery};

const SOURCE: &str = "Apple Music";
const SEARCH: &str = "https://lyrics-api.boidu.dev/getLyrics";
const TRUST: u32 = 180;
const AGENT: &str = concat!(
    "sonora/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/sonorahq/sonora)"
);

/// Apple Music word-timed TTML lyrics fetched through `lyrics-api.boidu.dev`.
pub struct Boidu {
    http: reqwest::Client,
}

impl Boidu {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    /// Queries the catalogue by title, artist, and length, narrowed to `album` when given. A
    /// cache miss on a specific album edition returns `401` or `404`, so a caller can retry
    /// without it.
    async fn find(&self, query: &LyricsQuery, album: Option<&str>) -> Result<Option<String>> {
        let duration = query.duration.as_secs().to_string();
        let mut params = vec![("s", query.title.as_str()), ("a", query.artist.as_str())];
        if let Some(album) = album {
            params.push(("al", album));
        }
        if !query.duration.is_zero() {
            params.push(("d", duration.as_str()));
        }

        let response = self
            .http
            .get(SEARCH)
            .query(&params)
            .header("User-Agent", AGENT)
            .send()
            .await
            .context("cannot reach the apple lyrics catalogue")?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::UNAUTHORIZED {
            return Ok(None);
        }
        if !status.is_success() {
            anyhow::bail!("the apple lyrics catalogue answered with status {status}");
        }
        let found: Answer = response
            .json()
            .await
            .context("cannot read the apple lyrics catalogue response")?;
        Ok(found.ttml.filter(|xml| !xml.trim().is_empty()))
    }
}

impl Default for Boidu {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct Answer {
    ttml: Option<String>,
}

#[async_trait]
impl LyricsProvider for Boidu {
    fn name(&self) -> &'static str {
        SOURCE
    }

    async fn search(&self, query: &LyricsQuery) -> Result<Vec<LyricsHit>> {
        let album = query.album.as_deref().filter(|album| !album.is_empty());
        let mut raw = self.find(query, album).await?;
        if raw.is_none() && album.is_some() {
            log::debug!(
                "lyrics: apple catalogue found no album match for {}, retrying without the album",
                query.title
            );
            raw = self.find(query, None).await?;
        }
        let Some(raw) = raw else {
            return Ok(Vec::new());
        };

        let sheet = ttml::parse(&raw)?;
        if sheet.lyrics.is_empty() {
            return Ok(Vec::new());
        }

        Ok(vec![LyricsHit {
            source: SOURCE,
            trust: TRUST,
            lyrics: sheet.lyrics,
            instrumental: false,
            title: query.title.clone(),
            artist: query.artist.clone(),
            album: query.album.clone(),
            duration: (!query.duration.is_zero()).then_some(query.duration),
            writers: sheet.writers,
        }])
    }
}
