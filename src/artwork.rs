//! Bounded artwork validation and reproducible, non-destructive derivatives.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::metadata::Cicp;
use image::{
    ConvertColorOptions, DynamicImage, GenericImageView as _, ImageEncoder as _, ImageFormat,
    ImageReader, Limits,
};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_DIMENSION: u32 = 16_384;
const MAX_PIXELS: u64 = 100_000_000;
const MAX_ALLOC: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ArtworkRole {
    Front,
    Back,
    Booklet,
    Disc,
    Obi,
    Spine,
    Other(String),
}

impl ArtworkRole {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Front => "front",
            Self::Back => "back",
            Self::Booklet => "booklet",
            Self::Disc => "disc",
            Self::Obi => "obi",
            Self::Spine => "spine",
            Self::Other(value) => value,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtworkProvenance {
    pub role: ArtworkRole,
    pub source_provider: Option<String>,
    pub source_reference: Option<String>,
    pub provider_release_id: Option<String>,
    pub exact_release: bool,
    pub rights: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ArtworkAssetMetadata {
    path: PathBuf,
    mime: String,
    width: u32,
    height: u32,
    provenance: ArtworkProvenance,
}

impl ArtworkAssetMetadata {
    #[must_use]
    pub fn from_validated(
        path: PathBuf,
        artwork: &ValidatedArtwork,
        provenance: ArtworkProvenance,
    ) -> Self {
        let (width, height) = artwork.dimensions();
        Self {
            path,
            mime: artwork.mime().to_owned(),
            width,
            height,
            provenance,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn mime(&self) -> &str {
        &self.mime
    }

    #[must_use]
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    #[must_use]
    pub const fn provenance(&self) -> &ArtworkProvenance {
        &self.provenance
    }
}

#[derive(Debug, Clone)]
pub struct ValidatedArtwork {
    bytes: Vec<u8>,
    format: ImageFormat,
    mime: &'static str,
    width: u32,
    height: u32,
    sha256: String,
    blake3: String,
}

impl ValidatedArtwork {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub const fn mime(&self) -> &str {
        self.mime
    }

    #[must_use]
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    #[must_use]
    pub fn blake3(&self) -> &str {
        &self.blake3
    }

    #[must_use]
    pub const fn extension(&self) -> &'static str {
        match self.format {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Png => "png",
            ImageFormat::Gif => "gif",
            ImageFormat::WebP => "webp",
            _ => "image",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtworkDerivative {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    source_sha256: String,
}

impl ArtworkDerivative {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Fully decode trusted formats under byte, pixel, dimension, and allocation limits.
pub fn validate_artwork(bytes: &[u8]) -> Result<ValidatedArtwork> {
    if bytes.is_empty() || bytes.len() > MAX_INPUT_BYTES {
        return Err(Error::Artwork(format!(
            "artwork must be between 1 and {MAX_INPUT_BYTES} bytes"
        )));
    }
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    reader.limits(image_limits());
    let format = reader
        .format()
        .ok_or_else(|| Error::Artwork("image format is unknown".into()))?;
    let image = reader.decode()?;
    let (width, height) = image.dimensions();
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .filter(|pixels| *pixels <= MAX_PIXELS)
        .ok_or_else(|| Error::Artwork("decoded artwork exceeds pixel limit".into()))?;
    if pixels == 0 {
        return Err(Error::Artwork("decoded artwork has zero dimensions".into()));
    }
    let mime = match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Png => "image/png",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => {
            return Err(Error::Artwork(
                "only JPEG, PNG, GIF, and WebP artwork is accepted".into(),
            ));
        }
    };
    let sha256 = crate::asset::sha256_bytes(bytes);
    let blake3 = blake3::hash(bytes).to_hex().to_string();
    Ok(ValidatedArtwork {
        bytes: bytes.to_vec(),
        format,
        mime,
        width,
        height,
        sha256,
        blake3,
    })
}

/// Build a deterministic PNG derivative. It is never cropped or upscaled.
pub fn make_derivative(original: &ValidatedArtwork, max_edge: u32) -> Result<ArtworkDerivative> {
    if max_edge == 0 || max_edge > MAX_DIMENSION {
        return Err(Error::Artwork("invalid derivative size".into()));
    }
    let mut image = decode_validated(original)?;
    image.apply_color_space(Cicp::SRGB, ConvertColorOptions::default())?;
    let (width, height) = image.dimensions();
    let derivative = if width <= max_edge && height <= max_edge {
        image
    } else {
        image.resize(max_edge, max_edge, image::imageops::FilterType::Lanczos3)
    };
    let (width, height) = derivative.dimensions();
    let mut output = Vec::new();
    let mut encoder =
        PngEncoder::new_with_quality(&mut output, CompressionType::Best, FilterType::Adaptive);
    let srgb_profile = moxcms::ColorProfile::new_srgb()
        .encode()
        .map_err(|error| Error::Artwork(format!("cannot encode the sRGB profile: {error}")))?;
    encoder.set_icc_profile(srgb_profile).map_err(|error| {
        Error::Artwork(format!(
            "cannot attach the sRGB profile to the derivative: {error}"
        ))
    })?;
    encoder.write_image(
        derivative.as_bytes(),
        width,
        height,
        derivative.color().into(),
    )?;
    Ok(ArtworkDerivative {
        bytes: output,
        width,
        height,
        source_sha256: original.sha256.clone(),
    })
}

/// Content-addressed location for an immutable original beneath the root.
#[must_use]
pub fn original_path(root: &Path, artwork: &ValidatedArtwork) -> PathBuf {
    root.join(".rsbts")
        .join("artwork")
        .join("original")
        .join(&artwork.sha256[..2])
        .join(format!("{}.{}", artwork.sha256, artwork.extension()))
}

fn decode_validated(original: &ValidatedArtwork) -> Result<DynamicImage> {
    let mut reader = ImageReader::with_format(Cursor::new(&original.bytes), original.format);
    reader.limits(image_limits());
    reader.decode().map_err(Into::into)
}

fn image_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_ALLOC);
    limits
}

impl PartialEq for ArtworkDerivative {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
            && self.width == other.width
            && self.height == other.height
            && self.source_sha256 == other.source_sha256
    }
}

#[cfg(test)]
mod tests {
    use image::codecs::png::PngDecoder;
    use image::{ImageBuffer, ImageDecoder as _, Rgb};

    use super::*;

    fn png(width: u32, height: u32) -> Result<Vec<u8>> {
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(width, height, Rgb([1, 2, 3])));
        let mut output = Cursor::new(Vec::new());
        image.write_to(&mut output, ImageFormat::Png)?;
        Ok(output.into_inner())
    }

    #[test]
    fn magic_bytes_without_a_decodable_image_are_rejected() {
        assert!(validate_artwork(b"\x89PNG\r\n\x1a\nnot-an-image").is_err());
    }

    #[test]
    fn originals_are_content_addressed_and_derivatives_are_reproducible() -> Result<()> {
        let original = validate_artwork(&png(32, 16)?)?;
        assert_eq!(original.mime(), "image/png");
        assert_eq!(original.dimensions(), (32, 16));
        let path = original_path(Path::new("/library"), &original);
        assert!(path.to_string_lossy().contains(original.sha256()));
        let first = make_derivative(&original, 8)?;
        let second = make_derivative(&original, 8)?;
        assert_eq!(first, second);
        assert_eq!(first.dimensions(), (8, 4));
        let mut decoder = PngDecoder::new(Cursor::new(first.bytes()))?;
        let icc = decoder
            .icc_profile()?
            .ok_or_else(|| Error::Artwork("derivative has no sRGB profile".into()))?;
        moxcms::ColorProfile::new_from_slice(&icc)
            .map_err(|error| Error::Artwork(format!("invalid derivative profile: {error}")))?;
        assert_eq!(
            icc,
            moxcms::ColorProfile::new_srgb().encode().map_err(|error| {
                Error::Artwork(format!("cannot encode expected sRGB profile: {error}"))
            })?
        );
        Ok(())
    }

    #[test]
    fn a_derivative_is_never_upscaled() -> Result<()> {
        let original = validate_artwork(&png(4, 2)?)?;
        assert_eq!(make_derivative(&original, 100)?.dimensions(), (4, 2));
        Ok(())
    }
}
