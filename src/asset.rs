//! Persistent file identity and fixity primitives.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::Result;

/// Whole-file digests calculated from one stable byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDigests {
    byte_size: u64,
    blake3: String,
    sha256: String,
}

impl FileDigests {
    /// Number of bytes included in both digests.
    #[must_use]
    pub const fn byte_size(&self) -> u64 {
        self.byte_size
    }

    /// Lowercase BLAKE3 digest used for fast operational verification.
    #[must_use]
    pub fn blake3(&self) -> &str {
        &self.blake3
    }

    /// Lowercase SHA-256 digest used for archival interoperability.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Hash a regular file once while calculating both required fixity digests.
pub fn digest_file(path: &Path) -> Result<FileDigests> {
    digest_reader(std::fs::File::open(path)?)
}

/// Hash one already-open stable byte stream. Callers that mutate beneath a
/// trusted directory handle use this to avoid reopening by an ambient path.
pub fn digest_reader(mut reader: impl Read) -> Result<FileDigests> {
    let mut blake3 = blake3::Hasher::new();
    let mut sha256 = Sha256::new();
    let mut byte_size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        blake3.update(&buffer[..read]);
        sha256.update(&buffer[..read]);
        byte_size = byte_size.saturating_add(read as u64);
    }
    Ok(FileDigests {
        byte_size,
        blake3: blake3.finalize().to_hex().to_string(),
        sha256: format!("{:x}", sha256.finalize()),
    })
}

#[must_use]
pub fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculates_both_digests_from_the_same_bytes() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("asset.bin");
        std::fs::write(&path, b"rsbts")?;
        let digests = digest_file(&path)?;
        assert_eq!(digests.byte_size(), 5);
        assert_eq!(digests.blake3(), blake3::hash(b"rsbts").to_hex().as_str());
        assert_eq!(
            digests.sha256(),
            "74763e8dc06746cb86626e63da4f54f712e37e8dbab71db1d6ce924b13d4e0f6"
        );
        Ok(())
    }
}
