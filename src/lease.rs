//! OS-backed collection leases.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Exclusive ownership of a library's recovery and mutation boundary.
///
/// The companion file is deliberately permanent. Removing lock files creates a
/// split-brain race between processes that hold the old inode and processes
/// that open a newly created one. Dropping this value closes the descriptor and
/// releases the operating-system lock, including after process termination.
#[derive(Debug)]
pub struct CollectionLease {
    _file: File,
    #[cfg(test)]
    path: PathBuf,
}

impl CollectionLease {
    pub(super) fn acquire_exclusive(database: &Path) -> Result<Self> {
        let path = lease_path(database);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        file.try_lock().map_err(|error| {
            Error::Lease(format!(
                "cannot acquire exclusive collection lease {}: {error}",
                path.display()
            ))
        })?;
        Ok(Self {
            _file: file,
            #[cfg(test)]
            path,
        })
    }

    #[cfg(test)]
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

fn lease_path(database: &Path) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(".lock");
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_exclusive_lease_fails_until_the_first_is_dropped() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let database = temporary.path().join("library.db");
        let first = CollectionLease::acquire_exclusive(&database)?;
        assert_eq!(first.path(), temporary.path().join("library.db.lock"));
        assert!(CollectionLease::acquire_exclusive(&database).is_err());
        drop(first);
        let _second = CollectionLease::acquire_exclusive(&database)?;
        Ok(())
    }
}
