//! Portable Unicode naming and deterministic edition signatures.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use unicode_casefold::UnicodeCaseFold as _;
use unicode_normalization::UnicodeNormalization as _;
use unicode_segmentation::UnicodeSegmentation as _;

use crate::{Error, Result};

const DEFAULT_COMPONENT_BYTES: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum NamingProfile {
    Portable,
    NativeFilesystem,
    Archival,
}

/// Stable exact-edition facts used for readable directory disambiguation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditionSignature {
    pub release_id: Option<String>,
    pub date: Option<String>,
    pub country: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub medium: Option<String>,
    pub barcode: Option<String>,
}

impl EditionSignature {
    #[must_use]
    pub fn display_suffix(&self) -> String {
        [
            self.date.as_deref(),
            self.country.as_deref(),
            self.label.as_deref(),
            self.catalog_number.as_deref(),
            self.medium.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" • ")
    }

    /// Stable suffix used only after two edition signatures share a name key.
    pub fn collision_suffix(&self) -> Result<String> {
        let bytes = serde_json::to_vec(self)?;
        Ok(blake3::hash(&bytes).to_hex()[..10].to_owned())
    }
}

/// Normalize display text to NFC and enforce the selected filesystem profile.
pub fn sanitize_component(value: &str, profile: NamingProfile) -> Result<String> {
    let mut normalized = value.nfc().collect::<String>();
    normalized = normalized
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(character, '/' | '\\' | '\0')
                || (profile != NamingProfile::NativeFilesystem
                    && matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
            {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    if profile != NamingProfile::NativeFilesystem {
        normalized = normalized.trim_end_matches([' ', '.']).to_owned();
    }
    if normalized.is_empty() || normalized == "." || normalized == ".." {
        normalized = "_".into();
    }
    if profile != NamingProfile::NativeFilesystem && is_windows_reserved(&normalized) {
        normalized.insert(0, '_');
    }
    truncate_graphemes(&mut normalized, DEFAULT_COMPONENT_BYTES);
    if normalized.is_empty() {
        return Err(Error::PathFormat("path component became empty".into()));
    }
    Ok(normalized)
}

/// NFKC plus full Unicode case folding for comparisons, never for display.
#[must_use]
pub fn collision_key(value: &str) -> String {
    value
        .nfkc()
        .collect::<String>()
        .as_str()
        .case_fold()
        .collect()
}

pub fn sanitize_relative_path(path: &Path, profile: NamingProfile) -> Result<PathBuf> {
    if path.is_absolute() {
        return Err(Error::PathFormat("rendered path must be relative".into()));
    }
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| Error::PathFormat("rendered path is not valid UTF-8".into()))?;
                output.push(sanitize_component(value, profile)?);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::PathFormat("rendered path escapes its root".into()));
            }
        }
    }
    if output.as_os_str().is_empty() {
        Err(Error::PathFormat("rendered path is empty".into()))
    } else {
        Ok(output)
    }
}

fn truncate_graphemes(value: &mut String, limit: usize) {
    if value.len() <= limit {
        return;
    }
    let mut output = String::new();
    for grapheme in value.graphemes(true) {
        if output.len() + grapheme.len() > limit {
            break;
        }
        output.push_str(grapheme);
    }
    *value = output;
}

fn is_windows_reserved(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value);
    matches!(
        collision_key(stem).as_str(),
        "con"
            | "prn"
            | "aux"
            | "nul"
            | "com1"
            | "com2"
            | "com3"
            | "com4"
            | "com5"
            | "com6"
            | "com7"
            | "com8"
            | "com9"
            | "lpt1"
            | "lpt2"
            | "lpt3"
            | "lpt4"
            | "lpt5"
            | "lpt6"
            | "lpt7"
            | "lpt8"
            | "lpt9"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unicode_not_nfc)]
    fn display_is_nfc_but_collision_keys_are_compatibility_folded() -> Result<()> {
        assert_eq!(
            sanitize_component("Cafe\u{301}", NamingProfile::Portable)?,
            "Café"
        );
        assert_eq!(collision_key("Straße"), collision_key("STRASSE"));
        assert_eq!(collision_key("K"), collision_key("K"));
        Ok(())
    }

    #[test]
    fn portable_names_handle_reserved_and_trailing_characters() -> Result<()> {
        assert_eq!(
            sanitize_component("CON.txt", NamingProfile::Portable)?,
            "_CON.txt"
        );
        assert_eq!(
            sanitize_component("album. ", NamingProfile::Portable)?,
            "album"
        );
        assert!(sanitize_relative_path(Path::new("../escape"), NamingProfile::Portable).is_err());
        Ok(())
    }

    #[test]
    fn truncation_never_splits_a_grapheme() -> Result<()> {
        let long = "👩‍🔬".repeat(100);
        let sanitized = sanitize_component(&long, NamingProfile::Portable)?;
        assert!(sanitized.len() <= DEFAULT_COMPONENT_BYTES);
        assert!(sanitized.ends_with("👩‍🔬"));
        Ok(())
    }

    #[test]
    fn edition_collision_suffix_is_deterministic() -> Result<()> {
        let edition = EditionSignature {
            release_id: Some("release-id".into()),
            country: Some("US".into()),
            ..EditionSignature::default()
        };
        assert_eq!(edition.collision_suffix()?, edition.collision_suffix()?);
        Ok(())
    }
}
