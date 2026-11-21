//! Saving one track as a file of its own, for other apps to play.
//!
//! A download stays the way the provider sent it, which only Sonora can open. An export
//! decodes the track through the same [`Fetch`](crate::engine::Fetch) a play uses and encodes
//! it again: MP3 through LAME at 256 kbps, or AAC-LC through FDK at 256 kbps in an MP4
//! container. Both are C libraries built from source with the crate, so they cross-compile
//! wherever `cc` does, Android included.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context as _, Result, anyhow, bail};
use bytes::Bytes;

/// What an export is saved as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Mp3,
    M4a,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Mp3 => "mp3",
            Format::M4a => "m4a",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Format::Mp3 => "audio/mpeg",
            Format::M4a => "audio/mp4",
        }
    }
}

/// The tags an export carries.
#[derive(Clone, Debug, Default)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    /// The front cover as an encoded image.
    pub cover: Option<Vec<u8>>,
}

/// The bit rate both encoders aim for, in kbps.
pub const KBPS: u32 = 256;

/// How many frames go to an encoder at a time. AAC takes exactly one frame's worth.
const AAC_FRAME: usize = 1024;
const MP3_CHUNK: usize = 4608;

/// Decodes all of `source` into `path` as `format`. It runs on a blocking thread, since reading
/// the source may wait on bytes still arriving.
pub(crate) fn encode<S>(source: S, format: Format, path: &Path) -> Result<()>
where
    S: rodio::Source<Item = f32>,
{
    let channels = source.channels().get() as usize;
    let rate = source.sample_rate().get();
    let frames = Stereo {
        source,
        channels,
        frame: Vec::with_capacity(channels),
    };
    match format {
        Format::Mp3 => mp3(frames, rate, path),
        Format::M4a => m4a(frames, rate, path),
    }
}

/// A source read as stereo frames: mono is doubled, and past two channels the front pair is
/// kept.
struct Stereo<S> {
    source: S,
    channels: usize,
    frame: Vec<f32>,
}

impl<S: Iterator<Item = f32>> Iterator for Stereo<S> {
    type Item = (f32, f32);

    fn next(&mut self) -> Option<(f32, f32)> {
        self.frame.clear();
        for _ in 0..self.channels.max(1) {
            self.frame.push(self.source.next()?);
        }
        let left = self.frame[0];
        let right = self.frame.get(1).copied().unwrap_or(left);
        Some((left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0)))
    }
}

fn mp3(mut frames: impl Iterator<Item = (f32, f32)>, rate: u32, path: &Path) -> Result<()> {
    use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, Quality, VbrMode};

    let mut builder = Builder::new().context("cannot start the MP3 encoder")?;
    builder
        .set_num_channels(2)
        .map_err(|error| anyhow!("MP3 channels: {error:?}"))?;
    builder
        .set_sample_rate(rate)
        .map_err(|error| anyhow!("MP3 sample rate {rate}: {error:?}"))?;
    builder
        .set_vbr_mode(VbrMode::Off)
        .map_err(|error| anyhow!("MP3 mode: {error:?}"))?;
    builder
        .set_brate(Bitrate::Kbps256)
        .map_err(|error| anyhow!("MP3 bit rate: {error:?}"))?;
    builder
        .set_quality(Quality::NearBest)
        .map_err(|error| anyhow!("MP3 quality: {error:?}"))?;
    builder
        .set_to_write_vbr_tag(false)
        .map_err(|error| anyhow!("MP3 tag: {error:?}"))?;
    let mut encoder = builder
        .build()
        .map_err(|error| anyhow!("cannot start the MP3 encoder: {error:?}"))?;

    let mut file = BufWriter::new(File::create(path).context("cannot create the MP3")?);
    let mut pcm = Vec::with_capacity(MP3_CHUNK * 2);
    let mut out = Vec::new();
    loop {
        pcm.clear();
        for (left, right) in frames.by_ref().take(MP3_CHUNK) {
            pcm.extend([left, right]);
        }
        if pcm.is_empty() {
            break;
        }
        out.clear();
        out.reserve(mp3lame_encoder::max_required_buffer_size(pcm.len() / 2));
        encoder
            .encode_to_vec(InterleavedPcm(&pcm), &mut out)
            .map_err(|error| anyhow!("cannot encode MP3: {error:?}"))?;
        file.write_all(&out).context("cannot write the MP3")?;
    }
    out.clear();
    out.reserve(7200);
    encoder
        .flush_to_vec::<FlushNoGap>(&mut out)
        .map_err(|error| anyhow!("cannot finish the MP3: {error:?}"))?;
    file.write_all(&out).context("cannot write the MP3")?;
    file.flush().context("cannot write the MP3")?;
    Ok(())
}

/// The rate AAC is encoded at for a source at `rate`, and how many source frames make one
/// encoded frame. A hi-res source comes down to 44.1 or 48 kHz, which is what every player
/// expects of an M4A.
fn aac_rate(rate: u32) -> Result<(u32, usize)> {
    match rate {
        8000 | 11025 | 12000 | 16000 | 22050 | 24000 | 32000 | 44100 | 48000 => Ok((rate, 1)),
        _ if rate % 44100 == 0 => Ok((44100, (rate / 44100) as usize)),
        _ if rate % 48000 == 0 => Ok((48000, (rate / 48000) as usize)),
        _ => bail!("cannot encode AAC at {rate} Hz"),
    }
}

fn freq_index(rate: u32) -> Result<mp4mux::SampleFreqIndex> {
    use mp4mux::SampleFreqIndex::*;
    Ok(match rate {
        48000 => Freq48000,
        44100 => Freq44100,
        32000 => Freq32000,
        24000 => Freq24000,
        22050 => Freq22050,
        16000 => Freq16000,
        12000 => Freq12000,
        11025 => Freq11025,
        8000 => Freq8000,
        _ => bail!("no AAC frequency index for {rate} Hz"),
    })
}

fn m4a(mut frames: impl Iterator<Item = (f32, f32)>, rate: u32, path: &Path) -> Result<()> {
    use fdk_aac::enc::{AudioObjectType, BitRate, ChannelMode, Encoder, EncoderParams, Transport};
    use mp4mux::{
        AacConfig, ChannelConfig, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig,
        TrackType,
    };

    let (rate, factor) = aac_rate(rate)?;
    let encoder = Encoder::new(EncoderParams {
        bit_rate: BitRate::Cbr(KBPS * 1000),
        sample_rate: rate,
        transport: Transport::Raw,
        channels: ChannelMode::Stereo,
        audio_object_type: AudioObjectType::Mpeg4LowComplexity,
    })
    .map_err(|error| anyhow!("cannot start the AAC encoder: {error}"))?;

    let file = BufWriter::new(File::create(path).context("cannot create the M4A")?);
    let brand = |name: &[u8; 4]| mp4mux::FourCC::from(*name);
    let config = Mp4Config {
        major_brand: brand(b"M4A "),
        minor_version: 512,
        compatible_brands: vec![
            brand(b"M4A "),
            brand(b"isom"),
            brand(b"iso2"),
            brand(b"mp41"),
        ],
        timescale: 1000,
    };
    let mut mp4 = Mp4Writer::write_start(file, &config).context("cannot start the M4A")?;
    mp4.add_track(&TrackConfig {
        track_type: TrackType::Audio,
        timescale: rate,
        language: "und".into(),
        media_conf: MediaConfig::AacConfig(AacConfig {
            bitrate: KBPS * 1000,
            profile: mp4mux::AudioObjectType::AacLowComplexity,
            freq_index: freq_index(rate)?,
            chan_conf: ChannelConfig::Stereo,
        }),
    })
    .context("cannot add the M4A track")?;

    let mut out = vec![0u8; 8192];
    let mut at = 0u64;
    let mut pcm: Vec<i16> = Vec::with_capacity(AAC_FRAME * 2);
    let mut feed = |pcm: &[i16], mp4: &mut Mp4Writer<BufWriter<File>>| -> Result<()> {
        let mut rest = pcm;
        loop {
            let info = encoder
                .encode(rest, &mut out)
                .map_err(|error| anyhow!("cannot encode AAC: {error}"))?;
            if info.output_size > 0 {
                mp4.write_sample(
                    1,
                    &Mp4Sample {
                        start_time: at,
                        duration: AAC_FRAME as u32,
                        rendering_offset: 0,
                        is_sync: true,
                        bytes: Bytes::copy_from_slice(&out[..info.output_size]),
                    },
                )
                .context("cannot write the M4A")?;
                at += AAC_FRAME as u64;
            }
            rest = &rest[info.input_consumed.min(rest.len())..];
            if rest.is_empty() || (info.input_consumed == 0 && info.output_size == 0) {
                return Ok(());
            }
        }
    };
    let to_i16 = |sample: f32| (sample * i16::MAX as f32).round() as i16;
    loop {
        pcm.clear();
        while pcm.len() < AAC_FRAME * 2 {
            let mut sum = (0.0, 0.0);
            let mut taken = 0;
            for (left, right) in frames.by_ref().take(factor) {
                sum.0 += left;
                sum.1 += right;
                taken += 1;
            }
            if taken == 0 {
                break;
            }
            pcm.push(to_i16(sum.0 / taken as f32));
            pcm.push(to_i16(sum.1 / taken as f32));
        }
        if pcm.is_empty() {
            break;
        }
        feed(&pcm, &mut mp4)?;
    }
    // FDK holds back about two frames; silence pushes the end of the song out.
    let silence = vec![0i16; AAC_FRAME * 2];
    for _ in 0..3 {
        feed(&silence, &mut mp4)?;
    }
    mp4.write_end().context("cannot finish the M4A")?;
    mp4.into_writer().flush().context("cannot write the M4A")?;
    Ok(())
}

/// Writes `tags` into the file at `path`: ID3v2 for an MP3, MP4 atoms for an M4A.
pub fn tag(path: &Path, tags: &Tags) -> Result<()> {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt as _;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::prelude::Accessor as _;
    use lofty::probe::Probe;
    use lofty::tag::{Tag, TagExt as _};

    let tagged = Probe::open(path)
        .context("cannot open the export")?
        .read()
        .context("cannot read the export")?;
    let mut tag = Tag::new(tagged.primary_tag_type());
    tag.set_title(tags.title.clone());
    if !tags.artist.is_empty() {
        tag.set_artist(tags.artist.clone());
    }
    if let Some(album) = tags.album.clone().filter(|album| !album.is_empty()) {
        tag.set_album(album);
    }
    if let Some(cover) = &tags.cover {
        let mime = match cover.get(..4) {
            Some([0x89, b'P', b'N', b'G']) => MimeType::Png,
            _ => MimeType::Jpeg,
        };
        tag.push_picture(
            Picture::unchecked(cover.clone())
                .pic_type(PictureType::CoverFront)
                .mime_type(mime)
                .build(),
        );
    }
    tag.save_to_path(path, WriteOptions::default())
        .context("cannot save the tags")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::NonZero;
    use std::time::Duration;

    use super::*;

    /// A second of a 440 Hz tone, as a decoder would yield it.
    fn tone(rate: u32, channels: u16) -> rodio::buffer::SamplesBuffer {
        let samples: Vec<f32> = (0..rate as usize)
            .flat_map(|at| {
                let value = (at as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5;
                std::iter::repeat_n(value, channels as usize)
            })
            .collect();
        rodio::buffer::SamplesBuffer::new(
            NonZero::new(channels).unwrap(),
            NonZero::new(rate).unwrap(),
            samples,
        )
    }

    fn length(path: &Path) -> Duration {
        use lofty::file::AudioFile as _;
        lofty::probe::Probe::open(path)
            .unwrap()
            .read()
            .unwrap()
            .properties()
            .duration()
    }

    #[test]
    fn exports_play_back_as_long_as_the_source_with_their_tags() {
        let dir = tempfile::tempdir().unwrap();
        let tags = Tags {
            title: "Song".into(),
            artist: "Artist".into(),
            album: Some("Album".into()),
            cover: None,
        };
        for (format, rate, channels) in [
            (Format::Mp3, 44100, 2),
            (Format::M4a, 44100, 2),
            (Format::M4a, 96000, 1),
            (Format::Mp3, 48000, 1),
        ] {
            let path = dir
                .path()
                .join(format!("song-{rate}.{}", format.extension()));
            encode(tone(rate, channels), format, &path).unwrap();
            tag(&path, &tags).unwrap();
            let seconds = length(&path).as_secs_f32();
            assert!(
                (0.9..1.3).contains(&seconds),
                "{format:?} at {rate}: {seconds}s"
            );
            use lofty::file::TaggedFileExt as _;
            use lofty::prelude::Accessor as _;
            let read = lofty::probe::Probe::open(&path).unwrap().read().unwrap();
            let kept = read.primary_tag().expect("a tag");
            assert_eq!(kept.title().as_deref(), Some("Song"));
            assert_eq!(kept.album().as_deref(), Some("Album"));
        }
    }
}
