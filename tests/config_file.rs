//! Loading a configuration from a TOML file.

use ogc_edr::config::Config;

/// Write a config to a scratch file and load it.
fn load(toml: &str) -> Result<Config, String> {
    let path = std::env::temp_dir().join(format!(
        "ogc-edr-config-{}-{:?}.toml",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&path, toml).expect("write config");
    let result = Config::from_file(&path).map_err(|e| e.description().to_string());
    let _ = std::fs::remove_file(&path);
    result
}

#[test]
fn a_collection_needs_only_an_id_and_a_location() {
    let config = load(
        r#"
        [[collections]]
        id = "era5"
        location = "gs://example/era5.zarr"
        "#,
    )
    .expect("minimal config should load");

    assert_eq!(config.collections.len(), 1);
    let collection = &config.collections[0];
    assert_eq!(collection.id, "era5");
    assert_eq!(collection.location, "gs://example/era5.zarr");
    // Everything else is discovered from the store, so it may be absent.
    assert!(collection.title.is_none());
    assert!(collection.description.is_none());
    assert!(collection.parameters.is_none());
    assert!(collection.keywords.is_empty());

    // Server-wide settings fall back to their defaults.
    assert_eq!(config.bind, "0.0.0.0:3000");
    assert_eq!(config.max_values, 5_000_000);
    assert_eq!(config.default_limit, 100_000);
    assert_eq!(config.base_url, "");
}

#[test]
fn several_stores_can_be_served_side_by_side() {
    let config = load(
        r#"
        bind = "127.0.0.1:8080"
        base_url = "https://edr.example.org"
        max_values = 1000
        default_limit = 500

        [[collections]]
        id = "era5"
        location = "gs://example/era5.zarr"
        title = "ERA5"
        keywords = ["reanalysis"]

        [[collections]]
        id = "era5-model-level"
        location = "gs://example/model-level.zarr"
        parameters = ["temperature", "specific_humidity"]
        "#,
    )
    .expect("multi-collection config should load");

    assert_eq!(config.bind, "127.0.0.1:8080");
    assert_eq!(config.base_url, "https://edr.example.org");
    assert_eq!(config.max_values, 1000);
    assert_eq!(config.default_limit, 500);

    let ids: Vec<&str> = config.collections.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["era5", "era5-model-level"]);
    assert_eq!(config.collections[0].keywords, ["reanalysis"]);
    assert_eq!(
        config.collections[1].parameters.as_deref(),
        Some(["temperature".to_string(), "specific_humidity".to_string()].as_slice())
    );
}

#[test]
fn the_shipped_example_is_valid() {
    // The file the README tells people to copy has to actually load.
    let config = Config::from_file("collections.example.toml").expect("example config");
    assert_eq!(config.collections.len(), 2);
    assert!(
        config
            .collections
            .iter()
            .all(|c| c.location.starts_with("gs://"))
    );
}

#[test]
fn a_config_serving_nothing_is_rejected() {
    let error = load("bind = \"0.0.0.0:3000\"").unwrap_err();
    assert!(error.contains("no collections configured"), "{error}");
}

#[test]
fn a_duplicate_id_is_rejected_rather_than_dropping_a_store() {
    // Ids key the catalogue, so the second would silently replace the first.
    let error = load(
        r#"
        [[collections]]
        id = "era5"
        location = "gs://example/one.zarr"

        [[collections]]
        id = "era5"
        location = "gs://example/two.zarr"
        "#,
    )
    .unwrap_err();
    assert!(error.contains("used more than once"), "{error}");
}

#[test]
fn an_id_that_would_not_survive_a_url_is_rejected() {
    // The id is a path segment in every URL the collection appears at.
    let error = load(
        r#"
        [[collections]]
        id = "era5/model level"
        location = "gs://example/era5.zarr"
        "#,
    )
    .unwrap_err();
    assert!(error.contains("URL path segment"), "{error}");
}

#[test]
fn a_missing_location_is_reported_against_its_collection() {
    let error = load(
        r#"
        [[collections]]
        id = "era5"
        location = ""
        "#,
    )
    .unwrap_err();
    assert!(
        error.contains("era5") && error.contains("location"),
        "{error}"
    );
}

#[test]
fn a_misspelled_key_is_reported_rather_than_ignored() {
    // Silently dropping `titel` would serve a collection with no title and no
    // hint as to why.
    let error = load(
        r#"
        [[collections]]
        id = "era5"
        location = "gs://example/era5.zarr"
        titel = "ERA5"
        "#,
    )
    .unwrap_err();
    assert!(
        error.contains("titel") || error.contains("unknown field"),
        "{error}"
    );
}

#[test]
fn a_malformed_file_names_the_file_and_the_place() {
    let error = load("[[collections]\nid = \"era5\"").unwrap_err();
    assert!(error.contains("invalid config"), "{error}");
    assert!(
        error.contains("ogc-edr-config"),
        "the file is not named: {error}"
    );
}

#[test]
fn a_missing_file_is_reported_clearly() {
    let error = Config::from_file("/nonexistent/collections.toml")
        .unwrap_err()
        .description()
        .to_string();
    assert!(error.contains("cannot read config"), "{error}");
    assert!(error.contains("/nonexistent/collections.toml"), "{error}");
}
