use rsbts::beets::BeetsMigration;
use rsbts::import::Action;

#[test]
fn beets_yaml_aliases_preserve_supported_configuration() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let source = temporary.path().join("beets.db");
    let config = temporary.path().join("config.yaml");
    let output = temporary.path().join("rsbts.db");
    let music = temporary.path().join("music");
    let connection = rusqlite::Connection::open(&source)?;
    connection.execute_batch(
        "CREATE TABLE albums (id INTEGER PRIMARY KEY);
         CREATE TABLE items (id INTEGER PRIMARY KEY);",
    )?;
    drop(connection);
    std::fs::write(
        &config,
        format!(
            "directory: {}\nsettings: &settings\n  copy: false\nimport: *settings\npaths:\n  default: '$artist/$title'\nplugins: [fetchart]\n",
            serde_json::to_string(&music)?
        ),
    )?;
    let source_before = std::fs::read(&source)?;
    let config_before = std::fs::read(&config)?;
    let migration = BeetsMigration::read(&source, Some(&config), None, output.clone())?;
    assert_eq!(migration.translated_config.library.directory, music);
    assert_eq!(migration.translated_config.import.action, Action::InPlace);
    assert_eq!(migration.translated_config.paths.format, "$artist/$title");
    assert_eq!(
        migration.report.warnings,
        ["Beets plugins are intentionally not migrated"]
    );
    assert_eq!(std::fs::read(source)?, source_before);
    assert_eq!(std::fs::read(config)?, config_before);
    assert!(!output.exists());
    assert!(!music.exists());
    Ok(())
}

#[test]
fn malformed_beets_yaml_fails_before_database_access() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let source = temporary.path().join("missing-beets.db");
    let config = temporary.path().join("config.yaml");
    let output = temporary.path().join("rsbts.db");
    std::fs::write(&config, "import: [invalid\n")?;
    let result = BeetsMigration::read(&source, Some(&config), None, output.clone());
    assert!(
        matches!(result, Err(rsbts::Error::Config(detail)) if detail.contains("invalid Beets YAML"))
    );
    assert!(!source.exists());
    assert!(!output.exists());
    Ok(())
}
