//! Side-effect-free configuration loading and validation.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::import::Action;
use crate::pathformat::validate_template;
use crate::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub library: LibraryConfig,
    pub paths: PathsConfig,
    pub import: ImportConfig,
    pub matching: MatchingConfig,
    pub musicbrainz: MusicBrainzConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LibraryConfig {
    pub directory: PathBuf,
    pub database: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PathsConfig {
    pub format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImportConfig {
    pub action: Action,
    pub fetch_art: bool,
    pub follow_symlinks: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MatchingConfig {
    pub auto_accept_threshold: f64,
    pub runner_up_margin: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MusicBrainzConfig {
    pub search_limit: u32,
    pub user_agent: String,
    pub rate_limit_seconds: f64,
    pub max_retries: u32,
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
                fetch_art: true,
                follow_symlinks: false,
            },
            matching: MatchingConfig {
                auto_accept_threshold: 0.92,
                runner_up_margin: 0.05,
            },
            musicbrainz: MusicBrainzConfig {
                search_limit: 5,
                user_agent: format!(
                    "rsbts/{} (https://github.com/sxndmxn/rsbts)",
                    env!("CARGO_PKG_VERSION")
                ),
                rate_limit_seconds: 1.0,
                max_retries: 3,
            },
        }
    }
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self::from(&Config::default())
    }
}

impl From<&Config> for LibraryConfig {
    fn from(config: &Config) -> Self {
        config.library.clone()
    }
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self::from(&Config::default())
    }
}

impl From<&Config> for PathsConfig {
    fn from(config: &Config) -> Self {
        config.paths.clone()
    }
}

impl Default for ImportConfig {
    fn default() -> Self {
        Self::from(&Config::default())
    }
}

impl From<&Config> for ImportConfig {
    fn from(config: &Config) -> Self {
        config.import.clone()
    }
}

impl Default for MatchingConfig {
    fn default() -> Self {
        Self::from(&Config::default())
    }
}

impl From<&Config> for MatchingConfig {
    fn from(config: &Config) -> Self {
        config.matching.clone()
    }
}

impl Default for MusicBrainzConfig {
    fn default() -> Self {
        Self::from(&Config::default())
    }
}

impl MusicBrainzConfig {
    /// Validate provider settings for CLI and direct library use.
    pub fn validate(&self) -> Result<()> {
        if self.search_limit == 0 {
            return Err(Error::Config(
                "musicbrainz.search_limit must be greater than zero".into(),
            ));
        }
        if self.search_limit > 100 {
            return Err(Error::Config(
                "musicbrainz.search_limit cannot be greater than 100".into(),
            ));
        }
        if self.user_agent.trim().is_empty() {
            return Err(Error::Config(
                "musicbrainz.user_agent cannot be empty".into(),
            ));
        }
        if !self.rate_limit_seconds.is_finite() || self.rate_limit_seconds < 1.0 {
            return Err(Error::Config(
                "musicbrainz.rate_limit_seconds must be finite and at least 1.0".into(),
            ));
        }
        if self.max_retries > 10 {
            return Err(Error::Config(
                "musicbrainz.max_retries cannot be greater than 10".into(),
            ));
        }
        Ok(())
    }
}

impl From<&Config> for MusicBrainzConfig {
    fn from(config: &Config) -> Self {
        config.musicbrainz.clone()
    }
}

impl Config {
    /// Load configuration without creating directories or opening the database.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let explicit_path = path.is_some();
        let config_path = path
            .map(PathBuf::from)
            .or_else(|| dirs::config_dir().map(|dir| dir.join("rsbts/config.toml")));
        let config_path = config_path.map(make_absolute).transpose()?;

        if explicit_path && config_path.as_ref().is_some_and(|path| !path.exists()) {
            return Err(Error::Config(format!(
                "config file does not exist: {}",
                config_path
                    .as_deref()
                    .map_or_else(String::new, |path| path.display().to_string())
            )));
        }

        let mut config = if let Some(candidate) = config_path.as_ref().filter(|p| p.exists()) {
            let content = std::fs::read_to_string(candidate)?;
            toml::from_str(&content).map_err(|error| Error::Config(error.to_string()))?
        } else {
            Self::default()
        };

        let base = config_path
            .as_deref()
            .and_then(Path::parent)
            .map_or_else(current_dir, |path| Ok(path.to_path_buf()))?;
        config.library.directory = resolve_config_path(&config.library.directory, &base)?;
        config.library.database = resolve_config_path(&config.library.database, &base)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.paths.format.trim().is_empty() {
            return Err(Error::Config("paths.format cannot be empty".into()));
        }
        validate_template(&self.paths.format)
            .map_err(|error| Error::Config(format!("invalid paths.format: {error}")))?;
        if !(0.0..=1.0).contains(&self.matching.auto_accept_threshold) {
            return Err(Error::Config(
                "matching.auto_accept_threshold must be between 0 and 1".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.matching.runner_up_margin) {
            return Err(Error::Config(
                "matching.runner_up_margin must be between 0 and 1".into(),
            ));
        }
        self.musicbrainz.validate()
    }
}

fn current_dir() -> Result<PathBuf> {
    std::env::current_dir().map_err(Error::from)
}

fn make_absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(current_dir()?.join(path))
    }
}

fn resolve_config_path(path: &Path, base: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    let expanded = if text == "~" {
        dirs::home_dir().ok_or_else(|| Error::Config("cannot resolve home directory".into()))?
    } else if let Some(remainder) = text.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or_else(|| Error::Config("cannot resolve home directory".into()))?
            .join(remainder)
    } else {
        path.to_path_buf()
    };

    Ok(if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_resolve_from_config_directory() -> Result<()> {
        let base = Path::new("/tmp/rsbts-config");
        assert_eq!(
            resolve_config_path(Path::new("data/library.db"), base)?,
            base.join("data/library.db")
        );
        Ok(())
    }

    #[test]
    fn loading_is_side_effect_free() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let config_path = temporary.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[library]\ndirectory = 'music'\ndatabase = 'data/library.db'\n",
        )?;
        let config = Config::load(Some(&config_path))?;
        assert_eq!(config.library.directory, temporary.path().join("music"));
        assert!(!temporary.path().join("data").exists());
        Ok(())
    }

    #[test]
    fn rejects_non_finite_rate_limits() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut config = Config::default();
            config.musicbrainz.rate_limit_seconds = value;
            assert!(config.validate().is_err());
        }
        let mut config = Config::default();
        config.musicbrainz.search_limit = 101;
        assert!(config.validate().is_err());
    }

    #[test]
    fn explicit_missing_config_fails_closed() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let missing = temporary.path().join("missing.toml");
        assert!(Config::load(Some(&missing)).is_err());
        Ok(())
    }

    #[test]
    fn unknown_config_fields_are_rejected() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let config_path = temporary.path().join("config.toml");
        std::fs::write(&config_path, "[import]\nacton = 'move'\n")?;
        assert!(Config::load(Some(&config_path)).is_err());
        Ok(())
    }

    #[test]
    fn relative_config_paths_become_absolute() -> Result<()> {
        let relative = PathBuf::from("config.toml");
        assert_eq!(make_absolute(relative)?, current_dir()?.join("config.toml"));
        Ok(())
    }

    #[test]
    fn malformed_path_templates_fail_during_config_loading() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let config_path = temporary.path().join("config.toml");
        std::fs::write(&config_path, "[paths]\nformat = '$artist/$titel'\n")?;
        assert!(matches!(
            Config::load(Some(&config_path)),
            Err(Error::Config(message)) if message.contains("unknown variable: titel")
        ));
        Ok(())
    }
}
