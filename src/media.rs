//! Content-derived media identification and decoded audio-essence fixity.

use std::fs::File;
use std::io::{BufReader, Read as _, Seek, SeekFrom};
use std::path::Path;

use blake3::Hasher;
use lofty::config::ParseOptions;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::mp4::{Mp4Codec, Mp4File};
use lofty::probe::Probe;
use lofty::tag::TagType;
use serde::{Deserialize, Serialize};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::{Error, Result};

const MAX_DECODED_PACKET_SAMPLES: usize = 16 * 1024 * 1024;

/// Physical container independent of codec and tag dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Container {
    Adts,
    Aiff,
    Ape,
    Flac,
    Mpeg,
    Mp4,
    Musepack,
    Ogg,
    Wave,
    WavPack,
    Opaque,
}

/// Encoded audio representation independent of its container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AudioCodec {
    Aac,
    Alac,
    MonkeyAudio,
    Flac,
    Mp3,
    Musepack,
    Opus,
    Pcm,
    Speex,
    Vorbis,
    WavPack,
    Unknown,
}

/// Native metadata encoding, modeled separately from media bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum TagDialect {
    ApeV2,
    Id3v1,
    Id3v2,
    Mp4Ilst,
    VorbisComments,
    RiffInfo,
    AiffText,
    None,
    Unknown,
}

/// Content-derived media facts retained in asset evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaDescriptor {
    container: Container,
    codec: AudioCodec,
    tag_dialect: TagDialect,
    duration_ms: u64,
    bitrate_kbps: Option<u32>,
    sample_rate_hz: Option<u32>,
    bit_depth: Option<u8>,
    channels: Option<u8>,
}

impl MediaDescriptor {
    #[must_use]
    pub const fn container(&self) -> Container {
        self.container
    }

    #[must_use]
    pub const fn codec(&self) -> AudioCodec {
        self.codec
    }

    #[must_use]
    pub const fn tag_dialect(&self) -> TagDialect {
        self.tag_dialect
    }
}

/// Published support contract for one media tuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct FormatCapability {
    pub container: Container,
    pub codec: AudioCodec,
    pub tag_dialect: TagDialect,
    pub read: bool,
    pub native_write: bool,
    pub multivalue: bool,
    pub artwork: bool,
    pub preserves_unknown: bool,
    pub validates_decoded_audio: bool,
    pub essence_validation: EssenceValidation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum EssenceValidation {
    DecodedPcm,
    EncodedAudioPackets,
}

/// Version of the format capability contract.
pub const FORMAT_CAPABILITY_VERSION: u32 = 2;

/// Initial capability matrix. Projection code refuses tuples absent here.
#[must_use]
pub fn format_capabilities() -> Vec<FormatCapability> {
    use AudioCodec::{
        Aac, Alac, Flac, MonkeyAudio, Mp3, Musepack, Opus, Pcm, Speex, Vorbis, WavPack,
    };
    use Container::{Adts, Aiff, Ape, Mp4, Mpeg, Musepack as Mpc, Ogg, Wave};
    use TagDialect::{AiffText, ApeV2, Id3v2, Mp4Ilst, RiffInfo, VorbisComments};

    [
        (Container::Flac, Flac, VorbisComments, true),
        (Mpeg, Mp3, Id3v2, true),
        (Ogg, Vorbis, VorbisComments, true),
        (Ogg, Opus, VorbisComments, false),
        (Ogg, Speex, VorbisComments, false),
        (Mp4, Aac, Mp4Ilst, true),
        (Mp4, Alac, Mp4Ilst, true),
        // Symphonia's ADTS reader does not safely handle every legal leading
        // ID3v2 layout. Hash the tag-stripped encoded access units instead.
        (Adts, Aac, Id3v2, false),
        (Wave, Pcm, Id3v2, true),
        (Aiff, Pcm, Id3v2, true),
        (Container::WavPack, WavPack, ApeV2, false),
        (Ape, MonkeyAudio, ApeV2, false),
        (Mpc, Musepack, ApeV2, false),
    ]
    .into_iter()
    .map(
        |(container, codec, tag_dialect, decoded)| FormatCapability {
            container,
            codec,
            tag_dialect,
            read: true,
            native_write: true,
            multivalue: !matches!(tag_dialect, Id3v2 | RiffInfo | AiffText),
            artwork: !matches!(tag_dialect, RiffInfo | AiffText),
            preserves_unknown: true,
            validates_decoded_audio: decoded,
            essence_validation: if decoded {
                EssenceValidation::DecodedPcm
            } else {
                EssenceValidation::EncodedAudioPackets
            },
        },
    )
    .collect()
}

/// Probe the actual bytes and return separate container, codec, and tag facts.
pub fn probe_media(path: &Path) -> Result<MediaDescriptor> {
    probe_media_from_file(File::open(path)?, path)
}

pub(crate) fn probe_media_from_file(file: File, path_hint: &Path) -> Result<MediaDescriptor> {
    let codec_reader = file.try_clone()?;
    let tagged = Probe::new(BufReader::new(file)).guess_file_type()?.read()?;
    let file_type = tagged.file_type();
    let properties = tagged.properties();
    let (container, codec) = classify(codec_reader, file_type, path_hint)?;
    let tag_dialect = tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())
        .map_or(TagDialect::None, |tag| tag_dialect(tag.tag_type()));
    Ok(MediaDescriptor {
        container,
        codec,
        tag_dialect,
        duration_ms: u64::try_from(properties.duration().as_millis()).unwrap_or(u64::MAX),
        bitrate_kbps: properties.audio_bitrate(),
        sample_rate_hz: properties.sample_rate(),
        bit_depth: properties.bit_depth(),
        channels: properties.channels(),
    })
}

/// Hash canonical interleaved decoded PCM samples, excluding tags and container metadata.
///
/// The stream is decoded packet-by-packet, so memory use is independent of file duration.
pub fn decoded_audio_essence_hash(path: &Path) -> Result<String> {
    decoded_audio_essence_hash_from_file(File::open(path)?, path)
}

pub(crate) fn decoded_audio_essence_hash_from_file(
    mut source: File,
    path_hint: &Path,
) -> Result<String> {
    let descriptor = probe_media_from_file(source.try_clone()?, path_hint)?;
    source.seek(SeekFrom::Start(0))?;
    if descriptor.container == Container::Ogg
        && matches!(descriptor.codec, AudioCodec::Opus | AudioCodec::Speex)
    {
        return ogg_audio_packet_hash(source);
    }
    if matches!(descriptor.container, Container::Wave | Container::Aiff) {
        return pcm_audio_chunk_hash(source, descriptor.container);
    }
    if matches!(
        descriptor.container,
        Container::Adts | Container::WavPack | Container::Ape | Container::Musepack
    ) {
        return tagged_payload_hash(source);
    }
    let stream = MediaSourceStream::new(Box::new(source), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(extension) = path_hint
        .extension()
        .and_then(|extension| extension.to_str())
    {
        hint.with_extension(extension);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| media_error(&error))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| Error::Media("file has no decodable audio track".into()))?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|error| media_error(&error))?;
    let mut hasher = Hasher::new();
    hasher.update(b"rsbts-decoded-pcm-f32-interleaved-v1\0");
    let mut decoded_any = false;
    let mut stream_specification = None;
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(error) => return Err(media_error(&error)),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let audio_buffer = decoder
            .decode(&packet)
            .map_err(|error| media_error(&error))?;
        let sample_count = audio_buffer
            .frames()
            .checked_mul(audio_buffer.spec().channels.count())
            .filter(|count| *count <= MAX_DECODED_PACKET_SAMPLES)
            .ok_or_else(|| Error::Media("decoded packet exceeds resource limit".into()))?;
        let specification = *audio_buffer.spec();
        let mut samples = SampleBuffer::<f32>::new(audio_buffer.capacity() as u64, specification);
        samples.copy_interleaved_ref(audio_buffer);
        if samples.samples().len() != sample_count {
            return Err(Error::Media(
                "decoder returned inconsistent sample count".into(),
            ));
        }
        let packet_specification = (specification.rate, specification.channels.count() as u32);
        if let Some(expected) = stream_specification {
            if packet_specification != expected {
                return Err(Error::Media(
                    "audio stream changed sample rate or channel count".into(),
                ));
            }
        } else {
            hasher.update(&packet_specification.0.to_le_bytes());
            hasher.update(&packet_specification.1.to_le_bytes());
            stream_specification = Some(packet_specification);
        }
        for sample in samples.samples() {
            hasher.update(&sample.to_bits().to_le_bytes());
        }
        decoded_any = true;
    }
    if decoded_any {
        Ok(hasher.finalize().to_hex().to_string())
    } else {
        Err(Error::Media("audio stream decoded no samples".into()))
    }
}

fn ogg_audio_packet_hash(mut source: File) -> Result<String> {
    source.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(source);
    let mut hasher = Hasher::new();
    hasher.update(b"rsbts-ogg-audio-packets-v1\0");
    let mut packet_index = 0_u64;
    let mut pages = 0_u64;
    loop {
        let mut header = [0_u8; 27];
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof && pages > 0 => break,
            Err(error) => return Err(error.into()),
        }
        if &header[..4] != b"OggS" || header[4] != 0 {
            return Err(Error::Media("invalid Ogg page header".into()));
        }
        pages = pages.saturating_add(1);
        let segment_count = usize::from(header[26]);
        let mut lacing = vec![0_u8; segment_count];
        reader.read_exact(&mut lacing)?;
        for length in lacing {
            let mut remaining = usize::from(length);
            if packet_index != 1 {
                hasher.update(&[length]);
            }
            let mut buffer = [0_u8; 16 * 1024];
            while remaining > 0 {
                let chunk = remaining.min(buffer.len());
                reader.read_exact(&mut buffer[..chunk])?;
                if packet_index != 1 {
                    hasher.update(&buffer[..chunk]);
                }
                remaining -= chunk;
            }
            if length < 255 {
                if packet_index != 1 {
                    hasher.update(b"\0packet\0");
                }
                packet_index = packet_index.saturating_add(1);
            }
        }
    }
    if packet_index < 3 {
        return Err(Error::Media("Ogg stream has no audio packets".into()));
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn pcm_audio_chunk_hash(mut source: File, container: Container) -> Result<String> {
    source.seek(SeekFrom::Start(0))?;
    let mut form = [0_u8; 12];
    source.read_exact(&mut form)?;
    let (expected_form, audio_chunk, little_endian) = match container {
        Container::Wave => (b"RIFF".as_slice(), b"data".as_slice(), true),
        Container::Aiff => (b"FORM".as_slice(), b"SSND".as_slice(), false),
        _ => return Err(Error::Media("container does not use PCM chunks".into())),
    };
    if &form[..4] != expected_form {
        return Err(Error::Media("invalid PCM container header".into()));
    }
    let mut hasher = Hasher::new();
    hasher.update(b"rsbts-pcm-sample-bytes-v1\0");
    loop {
        let mut chunk = [0_u8; 8];
        match source.read_exact(&mut chunk) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error.into()),
        }
        let size = if little_endian {
            u64::from(u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]))
        } else {
            u64::from(u32::from_be_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]))
        };
        if &chunk[..4] == audio_chunk {
            let mut payload_size = size;
            if container == Container::Aiff {
                if size < 8 {
                    return Err(Error::Media("invalid AIFF SSND chunk".into()));
                }
                let mut sound_header = [0_u8; 8];
                source.read_exact(&mut sound_header)?;
                let offset = u64::from(u32::from_be_bytes([
                    sound_header[0],
                    sound_header[1],
                    sound_header[2],
                    sound_header[3],
                ]));
                payload_size = size
                    .checked_sub(8)
                    .and_then(|remaining| remaining.checked_sub(offset))
                    .ok_or_else(|| Error::Media("invalid AIFF sound-data offset".into()))?;
                source.seek(SeekFrom::Current(i64::from(
                    u32::try_from(offset).map_err(|error| {
                        Error::Media(format!("AIFF sound-data offset is too large: {error}"))
                    })?,
                )))?;
            }
            hash_exact_bytes(&mut source, payload_size, &mut hasher)?;
            return Ok(hasher.finalize().to_hex().to_string());
        }
        let skip = size.saturating_add(size % 2);
        source.seek(SeekFrom::Current(i64::try_from(skip).map_err(|error| {
            Error::Media(format!("PCM chunk is too large: {error}"))
        })?))?;
    }
    Err(Error::Media("PCM container has no audio-data chunk".into()))
}

fn hash_exact_bytes(source: &mut File, mut remaining: u64, hasher: &mut Hasher) -> Result<()> {
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|error| Error::Media(format!("audio chunk is too large: {error}")))?;
        source.read_exact(&mut buffer[..chunk])?;
        hasher.update(&buffer[..chunk]);
        remaining -= chunk as u64;
    }
    Ok(())
}

fn tagged_payload_hash(mut source: File) -> Result<String> {
    let length = source.seek(SeekFrom::End(0))?;
    let mut start = 0_u64;
    let mut end = length;
    if length >= 10 {
        source.seek(SeekFrom::Start(0))?;
        let mut header = [0_u8; 10];
        source.read_exact(&mut header)?;
        if &header[..3] == b"ID3" {
            let size = synchsafe_u32(&header[6..10])?;
            start = 10_u64.saturating_add(u64::from(size));
        }
    }
    if end.saturating_sub(start) >= 128 {
        source.seek(SeekFrom::Start(end - 128))?;
        let mut marker = [0_u8; 3];
        source.read_exact(&mut marker)?;
        if &marker == b"TAG" {
            end -= 128;
        }
    }
    if end.saturating_sub(start) >= 32 {
        source.seek(SeekFrom::Start(end - 32))?;
        let mut footer = [0_u8; 32];
        source.read_exact(&mut footer)?;
        if &footer[..8] == b"APETAGEX" {
            let size = u64::from(u32::from_le_bytes([
                footer[12], footer[13], footer[14], footer[15],
            ]));
            if size < 32 || size > end.saturating_sub(start) {
                return Err(Error::Media("invalid APEv2 tag size".into()));
            }
            end -= size;
            if end.saturating_sub(start) >= 32 {
                source.seek(SeekFrom::Start(end - 32))?;
                let mut possible_header = [0_u8; 8];
                source.read_exact(&mut possible_header)?;
                if &possible_header == b"APETAGEX" {
                    end -= 32;
                }
            }
        }
    }
    if start >= end {
        return Err(Error::Media(
            "tagged file has no encoded audio payload".into(),
        ));
    }
    source.seek(SeekFrom::Start(start))?;
    let mut remaining = end - start;
    let mut hasher = Hasher::new();
    hasher.update(b"rsbts-encoded-audio-payload-v1\0");
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|error| Error::Media(format!("payload chunk is too large: {error}")))?;
        source.read_exact(&mut buffer[..chunk])?;
        hasher.update(&buffer[..chunk]);
        remaining -= chunk as u64;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn synchsafe_u32(bytes: &[u8]) -> Result<u32> {
    if bytes.len() != 4 || bytes.iter().any(|byte| byte & 0x80 != 0) {
        return Err(Error::Media("invalid ID3v2 synchsafe size".into()));
    }
    Ok(bytes
        .iter()
        .fold(0_u32, |value, byte| (value << 7) | u32::from(*byte)))
}

fn classify(
    codec_reader: File,
    file_type: FileType,
    path_hint: &Path,
) -> Result<(Container, AudioCodec)> {
    Ok(match file_type {
        FileType::Aac => (Container::Adts, AudioCodec::Aac),
        FileType::Aiff => (Container::Aiff, AudioCodec::Pcm),
        FileType::Ape => (Container::Ape, AudioCodec::MonkeyAudio),
        FileType::Flac => (Container::Flac, AudioCodec::Flac),
        FileType::Mpeg => (Container::Mpeg, AudioCodec::Mp3),
        FileType::Mp4 => (Container::Mp4, mp4_codec(codec_reader, path_hint)?),
        FileType::Mpc => (Container::Musepack, AudioCodec::Musepack),
        FileType::Opus => (Container::Ogg, AudioCodec::Opus),
        FileType::Vorbis => (Container::Ogg, AudioCodec::Vorbis),
        FileType::Speex => (Container::Ogg, AudioCodec::Speex),
        FileType::Wav => (Container::Wave, AudioCodec::Pcm),
        FileType::WavPack => (Container::WavPack, AudioCodec::WavPack),
        _ => (Container::Opaque, AudioCodec::Unknown),
    })
}

fn mp4_codec(mut file: File, path_hint: &Path) -> Result<AudioCodec> {
    file.rewind()
        .map_err(|error| Error::Media(format!("cannot seek {}: {error}", path_hint.display())))?;
    let parsed = Mp4File::read_from(&mut file, ParseOptions::new())?;
    Ok(match parsed.properties().codec() {
        Mp4Codec::AAC => AudioCodec::Aac,
        Mp4Codec::ALAC => AudioCodec::Alac,
        Mp4Codec::MP3 => AudioCodec::Mp3,
        Mp4Codec::FLAC => AudioCodec::Flac,
        _ => AudioCodec::Unknown,
    })
}

const fn tag_dialect(tag_type: TagType) -> TagDialect {
    match tag_type {
        TagType::Ape => TagDialect::ApeV2,
        TagType::Id3v1 => TagDialect::Id3v1,
        TagType::Id3v2 => TagDialect::Id3v2,
        TagType::Mp4Ilst => TagDialect::Mp4Ilst,
        TagType::VorbisComments => TagDialect::VorbisComments,
        TagType::RiffInfo => TagDialect::RiffInfo,
        TagType::AiffText => TagDialect::AiffText,
        _ => TagDialect::Unknown,
    }
}

fn media_error(error: &SymphoniaError) -> Error {
    Error::Media(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(samples: &[i16]) -> Vec<u8> {
        let data_len = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&16_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn descriptor_uses_content_and_separates_the_tuple() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("misnamed.mp3");
        std::fs::write(&path, wav(&[0, 1, -1]))?;
        let descriptor = probe_media(&path)?;
        assert_eq!(descriptor.container(), Container::Wave);
        assert_eq!(descriptor.codec(), AudioCodec::Pcm);
        Ok(())
    }

    #[test]
    fn decoded_essence_changes_with_samples_not_path() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let first = temporary.path().join("first.wav");
        let second = temporary.path().join("second.bin");
        let changed = temporary.path().join("changed.wav");
        std::fs::write(&first, wav(&[0, 1, -1]))?;
        std::fs::write(&second, wav(&[0, 1, -1]))?;
        std::fs::write(&changed, wav(&[0, 2, -1]))?;
        assert_eq!(
            decoded_audio_essence_hash(&first)?,
            decoded_audio_essence_hash(&second)?
        );
        assert_ne!(
            decoded_audio_essence_hash(&first)?,
            decoded_audio_essence_hash(&changed)?
        );
        Ok(())
    }

    #[test]
    fn capability_matrix_covers_every_initial_target() {
        assert_eq!(format_capabilities().len(), 13);
        assert!(format_capabilities().iter().all(|entry| entry.read));
    }
}
