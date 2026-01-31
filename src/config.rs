use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::import::Action;
use crate::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub library: LibraryConfig,
    pub paths: PathsConfig,
    pub import: ImportConfig,
    pub musicbrainz: MusicBrainzConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryConfig {
    pub directory: PathBuf,
    pub database: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathsConfig {
    pub format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportConfig {
    pub action: Action,
    pub write_tags: bool,
    pub fetch_art: bool,
    pub embed_art: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MusicBrainzConfig {
    pub search_limit: u32,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let data_dir = dirs::data_local_dir().unwrap_or_else(|| home.join(".local/share"));

        Self {
            library: LibraryConfig {
                directory: home.join("Music"),
                database: data_dir.join("rsbts/library.db"),
            },
            paths: PathsConfig {
                format: "$albumartist/$album/$track - $title".into(),
            },
            import: ImportConfig {
                action: Action::Copy,
                write_tags: true,
                fetch_art: true,
                embed_art: false,
            },
            musicbrainz: MusicBrainzConfig { search_limit: 5 },
        }
    }
}

impl Config {
    /// Load configuration from the given path or the default config location.
    ///
    /// # Errors
    /// Returns an error if the config file exists but cannot be read or parsed.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config_path = path
            .map(PathBuf::from)
            .or_else(|| dirs::config_dir().map(|d| d.join("rsbts/config.toml")));

        let config = if let Some(ref p) = config_path {
            if p.exists() {
                let content = std::fs::read_to_string(p)?;
                toml::from_str(&content).map_err(|e| crate::Error::Config(e.to_string()))?
            } else {
                Self::default()
            }
        } else {
            Self::default()
        };

        // Ensure database directory exists
        if let Some(parent) = config.library.database.parent() {
            std::fs::create_dir_all(parent)?;
        }

        Ok(config)
    }
}
