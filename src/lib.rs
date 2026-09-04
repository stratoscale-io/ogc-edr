//! An OGC API - Environmental Data Retrieval server over ERA5 held as Zarr.
//!
//! EDR data queries are translated into SQL over the Zarr store, which
//! [`zarr_datafusion`] pushes down to chunk reads.

pub mod api;
pub mod axis;
pub mod catalog;
pub mod config;
pub mod edr;
pub mod error;
