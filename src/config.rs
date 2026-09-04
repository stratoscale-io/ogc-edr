//! Server configuration, from a TOML file or the environment.
//!
//! A file is the way to serve more than one store; the environment path stays
//! as the zero-configuration default, which serves public ERA5.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{EdrError, EdrResult};

/// The public ARCO-ERA5 analysis-ready store: 0.25° hourly, surface variables
/// on (time, latitude, longitude) and 11 variables additionally on 37 pressure
/// levels.
pub const DEFAULT_ERA5_LOCATION: &str =
    "gs://gcp-public-data-arco-era5/ar/full_37-1h-0p25deg-chunk-1.zarr-v3";

/// One Zarr store, served as one EDR collection.
///
/// Only `id` and `location` are required: the axes, extents, resolution and
/// every parameter with its units are read from the store itself at startup.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionConfig {
    pub id: String,
    pub location: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Restrict the collection to these variables; `None` exposes all of them.
    #[serde(default)]
    pub parameters: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Prefix for link `href`s; empty means root-relative links.
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub collections: Vec<CollectionConfig>,
    /// Hard ceiling on grid cells × timesteps × parameters per request.
    #[serde(default = "default_max_values")]
    pub max_values: usize,
    /// `limit` applied when the request does not carry one.
    #[serde(default = "default_limit")]
    pub default_limit: usize,
}

fn default_bind() -> String {
    "0.0.0.0:3000".to_string()
}

fn default_max_values() -> usize {
    5_000_000
}

fn default_limit() -> usize {
    100_000
}

impl Config {
    /// The configuration this process should run with.
    ///
    /// `EDR_CONFIG` names a TOML file and settles it. Without one the
    /// environment is used, which serves a single ERA5 collection — the
    /// behaviour this server had before files existed, kept so an existing
    /// deployment does not need one.
    pub fn load() -> EdrResult<Self> {
        match std::env::var("EDR_CONFIG") {
            Ok(path) => Self::from_file(PathBuf::from(path)),
            Err(_) => Self::from_env(),
        }
    }

    /// Read and validate a TOML configuration.
    pub fn from_file(path: impl AsRef<Path>) -> EdrResult<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| {
            EdrError::Internal(format!("cannot read config '{}': {e}", path.display()))
        })?;
        let config: Self = toml::from_str(&text).map_err(|e| {
            // toml's message carries the line and column, which is the useful
            // part when a file is being hand-written.
            EdrError::Internal(format!("invalid config '{}': {e}", path.display()))
        })?;
        config.validate()?;
        Ok(config)
    }

    /// The single-collection default, driven by the environment.
    pub fn from_env() -> EdrResult<Self> {
        let location =
            std::env::var("ERA5_LOCATION").unwrap_or_else(|_| DEFAULT_ERA5_LOCATION.to_string());
        let parameters = std::env::var("ERA5_PARAMETERS").ok().map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        });
        // The title and description describe ERA5, so they are only right
        // while the location is an ERA5 store. Pointing `ERA5_LOCATION`
        // somewhere else is what the config file is for.
        let config = Self {
            bind: std::env::var("EDR_BIND").unwrap_or_else(|_| default_bind()),
            base_url: std::env::var("EDR_BASE_URL")
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_string(),
            max_values: parse_env("EDR_MAX_VALUES", default_max_values())?,
            default_limit: parse_env("EDR_DEFAULT_LIMIT", default_limit())?,
            collections: vec![CollectionConfig {
                id: std::env::var("ERA5_COLLECTION_ID").unwrap_or_else(|_| "era5".to_string()),
                title: Some("ERA5".to_string()),
                description: Some(
                    "ECMWF ERA5 global atmospheric reanalysis, analysis-ready \
                     0.25° hourly data served from a cloud-optimised Zarr store."
                        .to_string(),
                ),
                location,
                keywords: vec![
                    "ERA5".into(),
                    "reanalysis".into(),
                    "ECMWF".into(),
                    "climate".into(),
                    "weather".into(),
                ],
                parameters,
            }],
        };
        config.validate()?;
        Ok(config)
    }

    /// Reject a configuration that would start but could not serve.
    ///
    /// Checked before any store is opened, so a typo is reported in a second
    /// rather than after a minute of reading axes.
    fn validate(&self) -> EdrResult<()> {
        if self.collections.is_empty() {
            return Err(EdrError::Internal(
                "no collections configured: add at least one [[collections]] entry".into(),
            ));
        }
        for collection in &self.collections {
            if collection.id.trim().is_empty() {
                return Err(EdrError::Internal(
                    "a collection has an empty 'id'".to_string(),
                ));
            }
            // The id is a path segment in every URL the collection appears at.
            if !collection
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                return Err(EdrError::Internal(format!(
                    "collection id '{}' must contain only letters, digits, '-', '_' or '.': \
                     it is used as a URL path segment",
                    collection.id
                )));
            }
            if collection.location.trim().is_empty() {
                return Err(EdrError::Internal(format!(
                    "collection '{}' has no 'location'",
                    collection.id
                )));
            }
        }
        // Ids key the catalogue, so a duplicate would silently drop a store.
        let mut seen: Vec<&str> = self.collections.iter().map(|c| c.id.as_str()).collect();
        seen.sort_unstable();
        if let Some(duplicate) = seen.windows(2).find(|w| w[0] == w[1]) {
            return Err(EdrError::Internal(format!(
                "collection id '{}' is used more than once",
                duplicate[0]
            )));
        }
        if self.max_values == 0 || self.default_limit == 0 {
            return Err(EdrError::Internal(
                "'max_values' and 'default_limit' must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

fn parse_env(key: &str, default: usize) -> EdrResult<usize> {
    match std::env::var(key) {
        Ok(raw) => raw.trim().parse().map_err(|_| {
            EdrError::Internal(format!("{key} must be a positive integer, got {raw}"))
        }),
        Err(_) => Ok(default),
    }
}
