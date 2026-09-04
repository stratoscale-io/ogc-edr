//! Server configuration, from the environment.

use crate::error::{EdrError, EdrResult};

/// The public ARCO-ERA5 analysis-ready store: 0.25° hourly, surface variables
/// on (time, latitude, longitude) and 11 variables additionally on 37 pressure
/// levels.
pub const DEFAULT_ERA5_LOCATION: &str =
    "gs://gcp-public-data-arco-era5/ar/full_37-1h-0p25deg-chunk-1.zarr-v3";

#[derive(Debug, Clone)]
pub struct CollectionConfig {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub location: String,
    pub keywords: Vec<String>,
    /// Restrict the collection to these variables; `None` exposes all of them.
    pub parameters: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    /// Prefix for link `href`s; empty means root-relative links.
    pub base_url: String,
    pub collections: Vec<CollectionConfig>,
    /// Hard ceiling on grid cells × timesteps × parameters per request.
    pub max_values: usize,
    /// `limit` applied when the request does not carry one.
    pub default_limit: usize,
}

impl Config {
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

        Ok(Self {
            bind: std::env::var("EDR_BIND").unwrap_or_else(|_| "0.0.0.0:3000".to_string()),
            base_url: std::env::var("EDR_BASE_URL")
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_string(),
            max_values: parse_env("EDR_MAX_VALUES", 5_000_000)?,
            default_limit: parse_env("EDR_DEFAULT_LIMIT", 100_000)?,
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
        })
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
