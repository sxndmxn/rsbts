use std::collections::BTreeMap;

use serde::Deserialize;

const ALLOWED_STATUSES: &[&str] = &["missing", "partial", "parity", "divergence"];

#[derive(Debug, Deserialize)]
struct CompatibilityManifest {
    schema_version: u64,
    upstream_version: String,
    upstream_commit: String,
    platforms: Vec<String>,
    executable: String,
    plugin_scope: String,
    statuses: StatusVocabulary,
    subsystems: BTreeMap<String, Entry>,
}

#[derive(Debug, Deserialize)]
struct StatusVocabulary {
    allowed: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct UnixManifest {
    schema_version: u64,
    divergences: BTreeMap<String, Divergence>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    status: String,
    evidence: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Divergence {
    status: String,
    rule: String,
    evidence: Vec<String>,
}

fn assert_entry(name: &str, status: &str, evidence: &[String]) {
    assert!(
        ALLOWED_STATUSES.contains(&status),
        "{name} has unknown status {status}"
    );
    assert!(!evidence.is_empty(), "{name} must link to evidence");
    for path in evidence {
        assert!(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(path)
                .exists(),
            "{name} references missing evidence {path}"
        );
    }
}

#[test]
fn beets_compatibility_manifest_is_pinned_and_valid() -> Result<(), Box<dyn std::error::Error>> {
    let manifest: CompatibilityManifest =
        toml::from_str(include_str!("../compat/beets-2.11.toml"))?;

    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.upstream_version, "2.11.0");
    assert_eq!(
        manifest.upstream_commit,
        "26ab6b26361e8c9d77cdf04ba9cf5ca64bbbc722"
    );
    assert_eq!(manifest.platforms, ["linux", "macos"]);
    assert_eq!(manifest.executable, "rsbts");
    assert_eq!(manifest.plugin_scope, "bundled");
    assert_eq!(manifest.statuses.allowed, ALLOWED_STATUSES);
    assert!(!manifest.subsystems.is_empty());
    for (name, entry) in manifest.subsystems {
        assert_entry(&name, &entry.status, &entry.evidence);
    }
    Ok(())
}

#[test]
fn unix_divergence_manifest_is_valid() -> Result<(), Box<dyn std::error::Error>> {
    let manifest: UnixManifest = toml::from_str(include_str!("../compat/unix-divergences.toml"))?;

    assert_eq!(manifest.schema_version, 1);
    assert!(!manifest.divergences.is_empty());
    for (name, divergence) in manifest.divergences {
        assert!(!divergence.rule.trim().is_empty(), "{name} needs a rule");
        assert_entry(&name, &divergence.status, &divergence.evidence);
    }
    Ok(())
}
