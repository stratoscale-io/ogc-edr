# ogc-edr

An [OGC API - Environmental Data Retrieval](https://ogcapi.ogc.org/edr/) server
over ERA5 reanalysis held as cloud-optimised Zarr. EDR queries are translated
into SQL and executed by
[zarr-datafusion](https://github.com/stratoscale-io/zarr-datafusion), which pushes
coordinate predicates down to Zarr chunk reads.

```bash
cargo run                       # serves the public ARCO-ERA5 store on :3000
curl 'localhost:3000/collections/era5/position?coords=POINT(-0.13%2051.51)&parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z/2024-01-01T05:00:00Z'
```

## Endpoints

| Path | Purpose |
| --- | --- |
| `/` | Landing page |
| `/conformance` | Conformance classes |
| `/api` | OpenAPI 3.0 definition (JSON only) |
| `/static/style.{version}.css` | Vendored water.css plus overrides |
| `/collections` | Collection list |
| `/collections/{id}` | Extents, parameters, query endpoints |
| `/collections/{id}/position` | Data at one or more positions |
| `/collections/{id}/radius` | Data within a distance of a position |
| `/collections/{id}/area` | Data within a polygon |
| `/collections/{id}/cube` | Data within a bounding box |

`trajectory`, `corridor`, `items`, `locations` and `instances` are not
implemented and are not advertised in `/conformance`.

### HTML representation

Resources are served as HTML on the same URLs as the JSON, per OGC API Common's
HTML conformance class. `f=html` settles it outright; otherwise the `Accept`
header decides, so a browser gets a page and `curl` (which sends `*/*`) keeps
getting JSON. Ties go to JSON — this is an API that also renders pages.

Pages are built with [`maud`](https://maud.lambda.xyz/), checked at compile time
against the same structs the JSON is built from, and styled with
[water.css](https://watercss.kognise.dev/), vendored into the binary rather than
loaded from a CDN. Nothing is fetched from a third-party origin.

Every resource has an HTML representation: the landing page, `/conformance`,
`/collections`, `/collections/{id}` (extents, query endpoints, and all
parameters in a filtered table), and each of the four data queries.

The form uses native date-and-time pickers for `datetime`, bounded by the
collection's temporal extent and stepped to its cadence, so an instant outside
or between the data cannot be chosen. A picker holds one instant, so the form
carries two and the server composes them into EDR's `start/end`; `datetime`
itself always wins, leaving the API contract untouched, and links out of a
result page canonicalise back to it.

A data query URL with no geometry renders an empty form rather than an error,
and the form submits to itself with `GET` — so a result page's address *is* the
API request that produced it, one `f=` away from CoverageJSON or GeoJSON. A
rejected query comes back as the form with the reason and the submitted values
intact, instead of a JSON error body.

### Query parameters

- `coords` — WKT in CRS84: `POINT`, `MULTIPOINT`, `LINESTRING`, `POLYGON`
  (`cube` uses `bbox` instead).
- `parameter-name` — comma-separated. **Required**: the ERA5 collection exposes
  273 variables and has no sensible default.
- `datetime` — RFC 3339 instant or interval, `..` for an open end. Defaults to
  the latest available step.
- `z` — `500`, `850,700,500`, `500/850`, `R5/1000/-100`, or `all`. Omitted, a
  pressure-level parameter returns every level.
- `bbox` — `west,south,east,north`, or six values carrying min/max `z`.
- `within` / `within-units` — radius, in `m`, `km`, `mi`, `nmi` or `ft`.
- `resolution-x` / `-y` / `-z` — thin an axis to this many points, keeping both ends.
- `f` — `CoverageJSON` (default), `GeoJSON`, or `html`.
- `crs` — CRS84 only.
- `limit` — cap on the number of data values; oversized queries get a `413`
  explaining how to narrow them.

## Configuration

Two ways in. Without a config file the server reads the environment and serves
a single ERA5 collection, which is the zero-setup default:

| Variable | Default |
| --- | --- |
| `EDR_CONFIG` | unset (use the environment below) |
| `EDR_BIND` | `0.0.0.0:3000` |
| `EDR_BASE_URL` | empty (root-relative links) |
| `ERA5_LOCATION` | `gs://gcp-public-data-arco-era5/ar/full_37-1h-0p25deg-chunk-1.zarr-v3` |
| `ERA5_COLLECTION_ID` | `era5` |
| `ERA5_PARAMETERS` | all variables in the store |
| `EDR_MAX_VALUES` | `5000000` |
| `EDR_DEFAULT_LIMIT` | `100000` |

`EDR_CONFIG` points at a TOML file and settles it — everything above except
`EDR_CONFIG` itself is then ignored, and the file is the whole configuration.

### Serving more than one store

Use a file. `ERA5_LOCATION` can only describe one collection, and its title,
description and keywords are fixed strings that say "ERA5" whatever store you
aim it at.

**1. Write the file.** Copy [`collections.example.toml`](collections.example.toml)
and edit it. Only `id` and `location` are required per collection:

```toml
[[collections]]
id = "era5"
location = "gs://gcp-public-data-arco-era5/ar/full_37-1h-0p25deg-chunk-1.zarr-v3"
title = "ERA5"

[[collections]]
id = "era5-model-level"
location = "gs://gcp-public-data-arco-era5/ar/model-level-1h-0p25deg.zarr-v1"
title = "ERA5 model levels"
# Optional: serve a subset. Omit to expose every variable in the store.
parameters = ["temperature", "specific_humidity"]
```

Everything else about a collection is read from the store itself at startup —
axes, extents, resolution, and every parameter with its units and whether it
varies with height. `title`, `description` and `keywords` are the only things
the store cannot tell you.

**2. Point the server at it.**

```bash
EDR_CONFIG=collections.toml cargo run
```

Each collection appears at `/collections/{id}` with its own query endpoints,
and `/collections` lists them all.

The server-wide keys (`bind`, `base_url`, `max_values`, `default_limit`) go at
the top of the file, above the first `[[collections]]`.

### What a store has to look like

`location` may be a local path or `gs://`, `s3://` or `https://`. The store must
satisfy zarr-datafusion's data model: Zarr v2 or v3, coordinates as 1-D arrays,
and data variables forming a clean cartesian product of them.

The dimensions must also map onto EDR's four axes — x, y, optionally z, and
time. That covers analysis and reanalysis stores. A **forecast** archive
generally does not: it carries initialisation time *and* lead time, sometimes an
ensemble member too, and five dimensions do not fit four axes. Serving one means
either pinning the extras to a single value or implementing EDR *instances*
(`/collections/{id}/instances/{instanceId}`), which this server does not yet do.

### Startup cost

Nothing is served until every collection is open, so a bad `location` fails
fast rather than leaving a half-populated catalogue.

Each store is opened in its own task, so they load in parallel. Against the two
public stores in the example config, wall time to first response was 18-19s for
one collection and 23-25s for two (two runs each) — so a second store costs a
few seconds rather than doubling the wait. Tasks are what make that true:
futures joined on a single task still finish one after another here, because
the scan that reads an axis blocks its thread while it decompresses.

Configuration is validated before any store is opened, so a typo in an id or a
missing `location` is reported in a second rather than after a minute of reading
axes.

## What the ERA5 store forces

- **Longitudes are 0…360**, latitudes descend from the north pole. CRS84 input
  is folded onto the stored convention, results are presented back in CRS84,
  and axes are returned in ascending order — a box across the prime meridian
  takes cells from both ends of the axis.
- **Position snapping wraps.** A point at -0.1° is nearer to the 0° cell than to
  the 359.75° one, measured around the circle.
- **The time axis is pre-allocated** from 1900 to 2050, far beyond the populated
  data. The temporal extent comes from the store's `valid_time_start` /
  `valid_time_stop` attributes, with the stop date read as inclusive of its day.
- **Chunks span the globe.** One chunk holds a whole 721×1440 field (and all 37
  levels) at a single timestep, so a wider area costs no extra I/O while a
  longer `datetime` costs one chunk read per step per parameter. Expect seconds
  per timestep against the public bucket.
- **Surface and pressure-level variables cannot share a scan**, so requesting
  both runs one query per group and merges the results. Each parameter's
  CoverageJSON range declares its own `axisNames`.

## Backend constraints

`tests/zarr_backend.rs` pins the zarr-datafusion behaviour this server depends
on, against a fixture whose every value is known:

- Coordinate predicates are only ever emitted as `=` or `BETWEEN`. An `IN` list
  on an axis that is not the variable's outermost dimension returns values
  paired with the **wrong coordinates** — plausible-looking but misplaced data.
  Scattered selections are therefore widened to the range spanning them and
  trimmed when values are placed; `Selection::scan_row_count` bounds how far
  that widening can go.
- A surface variable's query must not name the vertical coordinate, or the scan
  declares a column it then omits from the batch.
- A projection mixing variables of different rank is rejected outright.

If any of those are fixed upstream, the corresponding test fails and says which
workaround can be removed.

## Tests

```bash
cargo test          # unit tests, plus end-to-end tests over a local Zarr fixture
```

`tests/fixture.rs` writes a small ERA5-shaped Zarr v2 store — wrapping 0…360
longitudes, descending latitudes, a `level` axis, CF time, and both surface and
pressure-level variables — so the full request path is exercised without
touching the network.
