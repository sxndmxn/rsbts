//! Library-root identity and filesystem capability contracts.

use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::Library;
use crate::{Error, Result};

/// Stable root identity independent of its mount path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RootId(String);

impl RootId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        Uuid::parse_str(&value)
            .map_err(|error| Error::Root(format!("invalid root ID: {error}")))?;
        Ok(Self(value.to_ascii_lowercase()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for RootId {
    fn default() -> Self {
        Self::new()
    }
}

impl<'de> Deserialize<'de> for RootId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum RootState {
    Online,
    Offline,
    ReadOnly,
    Degraded,
}

impl RootState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Offline => "offline",
            Self::ReadOnly => "read-only",
            Self::Degraded => "degraded",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "online" => Ok(Self::Online),
            "offline" => Ok(Self::Offline),
            "read-only" => Ok(Self::ReadOnly),
            "degraded" => Ok(Self::Degraded),
            _ => Err(Error::Root(format!("unknown root state: {value}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CaseBehavior {
    Sensitive,
    Insensitive,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum NormalizationBehavior {
    Preserves,
    Normalizes,
    Unknown,
}

/// Observed or conservatively inferred safety properties for a root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct RootCapabilities {
    schema: u32,
    state: RootState,
    read_only: bool,
    case_behavior: CaseBehavior,
    normalization_behavior: NormalizationBehavior,
    advisory_locking: bool,
    atomic_rename: bool,
    no_replace_rename: bool,
    symlink_safe_resolution: bool,
    max_component_bytes: usize,
    observed_at: String,
}

impl RootCapabilities {
    /// Inspect the root or its nearest existing ancestor without writing to it.
    pub fn detect(root: &Path) -> Result<Self> {
        if !root.is_absolute() {
            return Err(Error::Root("library root must be absolute".into()));
        }
        let existing = nearest_existing_ancestor(root)?;
        let metadata = std::fs::symlink_metadata(&existing)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Error::Root(format!(
                "root ancestor is not a real directory: {}",
                existing.display()
            )));
        }
        let read_only = metadata.permissions().readonly();
        let state = if !root.exists() && root != existing {
            if read_only {
                RootState::ReadOnly
            } else {
                RootState::Online
            }
        } else if read_only {
            RootState::ReadOnly
        } else {
            RootState::Online
        };
        Ok(Self {
            schema: 1,
            state,
            read_only,
            case_behavior: platform_case_behavior(),
            normalization_behavior: platform_normalization_behavior(),
            advisory_locking: true,
            atomic_rename: platform_has_atomic_rename(),
            no_replace_rename: platform_has_noreplace(),
            symlink_safe_resolution: platform_has_safe_resolution(),
            max_component_bytes: 255,
            observed_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    /// Fail closed unless all mutation guarantees are available.
    pub fn require_safe_mutation(&self) -> Result<()> {
        let safe = self.state == RootState::Online
            && !self.read_only
            && self.advisory_locking
            && self.atomic_rename
            && self.no_replace_rename
            && self.symlink_safe_resolution;
        if safe {
            Ok(())
        } else {
            Err(Error::Root(
                "root cannot provide locking, anchored resolution, atomic rename, and no-replace guarantees"
                    .into(),
            ))
        }
    }

    #[must_use]
    pub const fn state(&self) -> RootState {
        self.state
    }

    #[must_use]
    pub const fn case_behavior(&self) -> CaseBehavior {
        self.case_behavior
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryRoot {
    id: RootId,
    path: PathBuf,
    state: RootState,
    capabilities: RootCapabilities,
}

impl LibraryRoot {
    #[must_use]
    pub const fn id(&self) -> &RootId {
        &self.id
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn state(&self) -> RootState {
        self.state
    }

    #[must_use]
    pub const fn capabilities(&self) -> &RootCapabilities {
        &self.capabilities
    }
}

impl Library {
    /// Register a stable root UUID without claiming ownership of any files.
    pub fn register_root(&self, path: &Path) -> Result<LibraryRoot> {
        let path_text = root_path_text(path)?;
        if let Some(id) = self
            .conn
            .query_row(
                "SELECT id FROM library_roots WHERE path = ?1 AND state != 'legacy'",
                [path_text],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return self.library_root(&RootId::parse(id)?);
        }
        let capabilities = RootCapabilities::detect(path)?;
        let id = RootId::new();
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO library_roots
             (id, path, state, capabilities_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![
                id.as_str(),
                path_text,
                capabilities.state().as_str(),
                serde_json::to_string(&capabilities)?,
                now
            ],
        )?;
        Ok(LibraryRoot {
            id,
            path: path.to_path_buf(),
            state: capabilities.state(),
            capabilities,
        })
    }

    pub fn library_root(&self, id: &RootId) -> Result<LibraryRoot> {
        self.conn
            .query_row(
                "SELECT path, state, capabilities_json FROM library_roots
                 WHERE id = ?1 AND state != 'legacy'",
                [id.as_str()],
                |row| {
                    Ok((
                        PathBuf::from(row.get::<_, String>(0)?),
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| Error::Root("library root does not exist".into()))
            .and_then(|(path, state, capabilities)| {
                Ok(LibraryRoot {
                    id: id.clone(),
                    path,
                    state: RootState::parse(&state)?,
                    capabilities: serde_json::from_str(&capabilities)?,
                })
            })
    }

    pub fn library_roots_page(
        &self,
        after_id: Option<&RootId>,
        limit: u32,
    ) -> Result<Vec<LibraryRoot>> {
        if limit == 0 || limit > 4096 {
            return Err(Error::Root(
                "library-root limit must be between 1 and 4096".into(),
            ));
        }
        let mut statement = self.conn.prepare(
            "SELECT id, path, state, capabilities_json FROM library_roots
             WHERE state != 'legacy' AND id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let roots = statement
            .query_map(params![after_id.map_or("", RootId::as_str), limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    PathBuf::from(row.get::<_, String>(1)?),
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .map(|row| {
                let (id, path, state, capabilities) = row?;
                Ok(LibraryRoot {
                    id: RootId::parse(id)?,
                    path,
                    state: RootState::parse(&state)?,
                    capabilities: serde_json::from_str(&capabilities)?,
                })
            })
            .collect();
        roots
    }

    /// Change availability policy. Returning online always refreshes capabilities.
    pub fn set_root_state(&self, id: &RootId, state: RootState) -> Result<LibraryRoot> {
        let current = self.library_root(id)?;
        let capabilities = if state == RootState::Online {
            let detected = RootCapabilities::detect(current.path())?;
            if detected.state() != RootState::Online {
                return Err(Error::Root(
                    "root cannot be marked online because it is unavailable or read-only".into(),
                ));
            }
            detected
        } else {
            current.capabilities
        };
        let changed = self.conn.execute(
            "UPDATE library_roots
             SET state = ?2, capabilities_json = ?3, updated_at = ?4
             WHERE id = ?1 AND state != 'legacy'",
            params![
                id.as_str(),
                state.as_str(),
                serde_json::to_string(&capabilities)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        if changed != 1 {
            return Err(Error::Root("library root state update was stale".into()));
        }
        Ok(LibraryRoot {
            id: id.clone(),
            path: current.path,
            state,
            capabilities,
        })
    }
}

fn root_path_text(path: &Path) -> Result<&str> {
    if !path.is_absolute() {
        return Err(Error::Root("library root path must be absolute".into()));
    }
    path.to_str()
        .ok_or_else(|| Error::Root("library root path must be valid UTF-8".into()))
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut current = path;
    loop {
        if current.exists() || current.is_symlink() {
            return Ok(current.to_path_buf());
        }
        current = current
            .parent()
            .ok_or_else(|| Error::Root("root has no existing ancestor".into()))?;
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const fn platform_case_behavior() -> CaseBehavior {
    CaseBehavior::Sensitive
}

#[cfg(windows)]
const fn platform_case_behavior() -> CaseBehavior {
    CaseBehavior::Insensitive
}

#[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
const fn platform_case_behavior() -> CaseBehavior {
    CaseBehavior::Unknown
}

#[cfg(any(target_os = "linux", target_os = "android", windows))]
const fn platform_normalization_behavior() -> NormalizationBehavior {
    NormalizationBehavior::Preserves
}

#[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
const fn platform_normalization_behavior() -> NormalizationBehavior {
    NormalizationBehavior::Unknown
}

#[cfg(any(target_os = "linux", target_os = "android", windows))]
const fn platform_has_atomic_rename() -> bool {
    true
}

#[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
const fn platform_has_atomic_rename() -> bool {
    false
}

#[cfg(any(target_os = "linux", target_os = "android", windows))]
const fn platform_has_noreplace() -> bool {
    true
}

#[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
const fn platform_has_noreplace() -> bool {
    false
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const fn platform_has_safe_resolution() -> bool {
    true
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
const fn platform_has_safe_resolution() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_root_exposes_a_serializable_capability_profile() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let capabilities = RootCapabilities::detect(temporary.path())?;
        #[cfg(target_os = "linux")]
        capabilities.require_safe_mutation()?;
        let json = serde_json::to_string(&capabilities)?;
        assert!(json.contains("no_replace_rename"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_root_is_rejected() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let link = temporary.path().join("link");
        std::os::unix::fs::symlink(temporary.path(), &link)?;
        assert!(RootCapabilities::detect(&link).is_err());
        Ok(())
    }

    #[test]
    fn root_identity_survives_explicit_availability_changes() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let library = Library::open_in_memory()?;
        let registered = library.register_root(temporary.path())?;
        assert_eq!(registered.state(), RootState::Online);
        let offline = library.set_root_state(registered.id(), RootState::Offline)?;
        assert_eq!(offline.id(), registered.id());
        assert_eq!(offline.state(), RootState::Offline);
        let online = library.set_root_state(registered.id(), RootState::Online)?;
        assert_eq!(online.id(), registered.id());
        assert_eq!(online.state(), RootState::Online);
        assert_eq!(library.library_roots_page(None, 10)?.len(), 1);
        Ok(())
    }
}
