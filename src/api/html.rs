//! The HTML representation: a shared page shell and the pages built on it.
//!
//! Markup is written with `maud`, so it is checked at compile time against the
//! same structs the JSON responses are built from and there are no template
//! files to keep in step. Styling is water.css, vendored and served from this
//! binary rather than a CDN so the server stays self-contained.

use std::sync::LazyLock;

use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::api::AppState;
use crate::api::metadata::{CONFORMANCE, OUTPUT_FORMATS, QUERY_TYPES};
use crate::catalog::Collection;
use crate::edr::params::Query;
use crate::edr::query::{DataRequest, QueryKind, ResultSet, format_time};

/// water.css, vendored (MIT, <https://github.com/kognise/water.css>), followed
/// by this application's own rules.
pub const STYLESHEET: &str = concat!(
    include_str!("../../assets/water.css"),
    "\n",
    include_str!("../../assets/app.css"),
);

/// Path the stylesheet is served from, fingerprinted with a hash of its own
/// content.
///
/// The response is cached hard, so the URL has to change whenever the bytes do
/// — otherwise an edit stays invisible behind a year-long cache entry. Keying
/// on the crate version would not do it: the stylesheet changes far more often
/// than the version does.
pub static STYLESHEET_PATH: LazyLock<String> =
    LazyLock::new(|| format!("/static/style.{:016x}.css", fingerprint(STYLESHEET)));

/// FNV-1a. Not a security hash — it only has to change when the input does,
/// and it saves a dependency for something this small.
const fn fingerprint(text: &str) -> u64 {
    let bytes = text.as_bytes();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    hash
}

/// Narrows a long list as you type.
///
/// An input carrying `data-filter="<id>"` filters the `[data-search]` elements
/// inside the container with that id, and reports the tally into any
/// `[data-filter-count="<id>"]`. The parameter table and the parameter
/// checkboxes are both long lists, and both use this.
///
/// The list is rendered in full and narrowed in the browser: a few hundred
/// entries are well within what a page can hold, and filtering here avoids a
/// round trip per keystroke. With scripting off the list is simply unfiltered,
/// which is why the control reveals itself rather than sitting in the markup.
const FILTER_JS: &str = r#"
(function () {
  function bind(box) {
    var container = document.getElementById(box.dataset.filter);
    if (!container) return;
    var items = Array.prototype.slice.call(container.querySelectorAll('[data-search]'));
    var tally = document.querySelector('[data-filter-count="' + box.dataset.filter + '"]');
    var noun = box.dataset.filterNoun || 'items';
    function apply() {
      var needle = box.value.trim().toLowerCase();
      var shown = 0;
      items.forEach(function (item) {
        var match = !needle || item.dataset.search.indexOf(needle) !== -1;
        item.hidden = !match;
        if (match) shown++;
      });
      if (tally) {
        var text = shown === items.length
          ? items.length + ' ' + noun
          : shown + ' of ' + items.length + ' ' + noun;
        // A checked box that the filter has hidden is still submitted, so the
        // count says how many are selected rather than leaving it a surprise.
        var checked = container.querySelectorAll('input:checked').length;
        tally.textContent = checked ? text + ' · ' + checked + ' selected' : text;
      }
    }
    box.addEventListener('input', apply);
    container.addEventListener('change', apply);
    if (box.parentElement) box.parentElement.hidden = false;
    apply();
  }
  Array.prototype.forEach.call(document.querySelectorAll('[data-filter]'), bind);
})();
"#;

/// Copies a command to the clipboard.
///
/// A button carrying `data-copy="<id>"` copies the text of the element with
/// that id. Revealed by the script, so a browser without one is not offered a
/// button that cannot work — the command is still there to select by hand.
const COPY_JS: &str = r#"
(function () {
  function copy(text) {
    // The async clipboard API needs a secure context, which this server may
    // well not have when it is running over plain HTTP on a LAN.
    if (navigator.clipboard && navigator.clipboard.writeText) {
      return navigator.clipboard.writeText(text);
    }
    return new Promise(function (resolve, reject) {
      var area = document.createElement('textarea');
      area.value = text;
      area.setAttribute('readonly', '');
      area.style.position = 'fixed';
      area.style.left = '-9999px';
      document.body.appendChild(area);
      area.select();
      try {
        document.execCommand('copy') ? resolve() : reject();
      } catch (e) {
        reject(e);
      } finally {
        document.body.removeChild(area);
      }
    });
  }
  Array.prototype.forEach.call(document.querySelectorAll('[data-copy]'), function (button) {
    var source = document.getElementById(button.dataset.copy);
    if (!source) return;
    button.hidden = false;
    var idle = button.textContent;
    var restore;
    button.addEventListener('click', function () {
      copy(source.textContent).then(
        function () { button.textContent = 'Copied'; },
        function () { button.textContent = 'Press Ctrl-C'; }
      ).then(function () {
        clearTimeout(restore);
        restore = setTimeout(function () { button.textContent = idle; }, 1500);
      });
    });
  });
})();
"#;

/// A shell command, with a button to copy it.
///
/// The button copies the text of the `<code>` itself, so what lands on the
/// clipboard is exactly what is on the page — it cannot drift from it.
fn command(id: &str, command: &str) -> Markup {
    html! {
        div.command {
            button.copy type="button" hidden data-copy=(id) { "Copy" }
            pre { code id=(id) { (command) } }
        }
    }
}

/// The filter control for a list, hidden until the script reveals it.
fn filter_control(target: &str, noun: &str, placeholder: &str) -> Markup {
    html! {
        p.filter hidden {
            label for=(format!("filter-{target}")) { "Filter " }
            input #(format!("filter-{target}")) type="search" autocomplete="off"
                  placeholder=(placeholder)
                  data-filter=(target) data-filter-noun=(noun);
            " " span.muted data-filter-count=(target) {}
        }
    }
}

/// One breadcrumb entry: a label, and the href to link it to unless it is the
/// current page.
pub struct Crumb {
    pub label: String,
    pub href: Option<String>,
}

impl Crumb {
    pub fn link(label: impl Into<String>, href: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            href: Some(href.into()),
        }
    }

    pub fn current(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            href: None,
        }
    }
}

/// The shell every page shares: head, breadcrumbs, content, colophon.
pub fn page(state: &AppState, title: &str, crumbs: &[Crumb], content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " — ERA5 EDR" }
                link rel="stylesheet" href=(state.href(&STYLESHEET_PATH));
            }
            body {
                @if !crumbs.is_empty() {
                    nav.crumbs aria-label="Breadcrumb" {
                        ol {
                            @for crumb in crumbs {
                                li {
                                    @match &crumb.href {
                                        Some(href) => a href=(state.href(href)) { (crumb.label) },
                                        None => span aria-current="page" { (crumb.label) },
                                    }
                                }
                            }
                        }
                    }
                }
                main { (content) }
                script { (PreEscaped(FILTER_JS)) (PreEscaped(COPY_JS)) }
                footer.colophon {
                    p {
                        "Every page here is also an API response. Add "
                        code { "f=json" }
                        " to any URL, or request it with "
                        code { "Accept: application/json" }
                        ", to get the OGC API - EDR representation."
                    }
                    p {
                        "Served by "
                        a href="https://github.com/jayendra13/zarr-datafusion" { "zarr-datafusion" }
                        " — SQL over Zarr, pushed down to chunk reads."
                    }
                }
            }
        }
    }
}

/// The landing page: what this service is, and the way in.
///
/// `host` is the request's own `Host` header, used to make the example command
/// runnable as printed.
pub fn landing(state: &AppState, host: Option<&str>) -> Markup {
    let collections = &state.catalog.collections;

    let content = html! {
        header.masthead {
            h1 { "ERA5 Environmental Data Retrieval" }
            p.tagline {
                "OGC API - EDR over ECMWF ERA5 reanalysis, read directly from "
                "cloud-optimised Zarr."
            }
        }

        ul.resources {
            li {
                a href=(state.href("/collections")) { "Collections" }
                span.badge { "data" }
                p {
                    @match collections.len() {
                        // Naming the collection is more use than a count when
                        // there is only one to look at.
                        1 => {
                            @let collection = collections.values().next().expect("one collection");
                            (collection.title) " — " (collection.parameters.len())
                            " parameters on a "
                            (collection.x.len()) "×" (collection.y.len()) " grid."
                        }
                        n => { (n) " collections available to query." }
                    }
                }
            }
            li {
                a href=(state.href("/conformance")) { "Conformance" }
                span.badge { "conformance" }
                p { "The OGC conformance classes this server implements." }
            }
            li {
                a href=(state.href("/api")) { "API definition" }
                span.badge { "service-desc" }
                p { "OpenAPI 3.0 description of every endpoint and parameter." }
            }
        }

        h2 { "Querying" }
        p {
            "Data is retrieved by geometry: a "
            b { "position" } " or several, everything within a " b { "radius" }
            ", inside an " b { "area" } ", or across a " b { "cube" }
            ". Responses are CoverageJSON by default, or GeoJSON with "
            code { "f=GeoJSON" } "."
        }
        @if let Some(collection) = collections.values().next() {
            p {
                "Start from "
                a href=(state.href(&format!("/collections/{}", collection.id))) {
                    "the " (collection.title) " collection"
                }
                ", which lists its parameters, extents and query endpoints."
            }
            @let example = format!(
                "curl -G '{}' \\\n  \
                 --data-urlencode 'coords=POINT(-0.13 51.51)' \\\n  \
                 --data-urlencode 'parameter-name=2m_temperature' \\\n  \
                 --data-urlencode 'datetime={}'",
                state.absolute_href(&format!("/collections/{}/position", collection.id), host),
                format_time(collection.time_extent().1),
            );
            (command("example-query", &example))
        }
    };

    page(state, "Home", &[], content)
}

/// An instant as a `datetime-local` input wants it: `YYYY-MM-DDTHH:MM`, no
/// zone marker. The field is documented as UTC, which is what the store uses.
fn picker_time(micros: i64) -> String {
    chrono::DateTime::from_timestamp_micros(micros)
        .map(|t| t.format("%Y-%m-%dT%H:%M").to_string())
        .unwrap_or_default()
}

/// The two picker values, from whichever spelling the request carried.
///
/// A link built for the API says `datetime=start/end`, and the form says
/// `datetime-from`/`datetime-to`; arriving by either route must fill the same
/// two boxes.
fn datetime_bounds(values: &Query) -> (String, String) {
    let as_picker = |raw: &str| {
        let raw = raw.trim();
        if raw.is_empty() || raw == ".." {
            return String::new();
        }
        // Reformat so a full RFC 3339 timestamp lands in a field that only
        // accepts minutes; anything unparseable is handed back for correction.
        crate::edr::params::parse_instant(raw)
            .map(picker_time)
            .unwrap_or_else(|_| raw.to_string())
    };

    if let Some(raw) = values.get("datetime") {
        let (from, to) = raw.split_once('/').unwrap_or((raw, raw));
        return (as_picker(from), as_picker(to));
    }
    (
        as_picker(values.get("datetime-from").unwrap_or_default()),
        as_picker(values.get("datetime-to").unwrap_or_default()),
    )
}

/// Format a coordinate or level without trailing zeros.
fn number(value: f64) -> String {
    let text = format!("{value:.6}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() || text == "-" {
        "0".to_string()
    } else {
        text.to_string()
    }
}

/// A link to the JSON representation of the page being viewed.
fn as_json(state: &AppState, path: &str) -> Markup {
    html! {
        p.alternate {
            "This page as data: "
            a href=(state.href(&format!("{path}?f=json"))) { code { "f=json" } }
        }
    }
}

/// `/conformance`: the OGC classes this server implements.
pub fn conformance(state: &AppState) -> Markup {
    let content = html! {
        header.masthead {
            h1 { "Conformance" }
            p.tagline {
                "The " (CONFORMANCE.len()) " OGC API classes this server implements. "
                "Classes it does not implement are absent rather than declared and unsupported."
            }
        }

        ul.classes {
            @for class in CONFORMANCE {
                li {
                    // The last path segment is the class name; the rest is the
                    // specification it belongs to.
                    @let name = class.rsplit('/').next().unwrap_or(class);
                    b { (name) }
                    br;
                    code { (class) }
                }
            }
        }

        h2 { "Not implemented" }
        p {
            "The EDR query types "
            code { "trajectory" } ", " code { "corridor" } ", " code { "items" } ", "
            code { "locations" } " and " code { "instances" }
            " have no endpoint here, so their conformance classes are not declared."
        }

        (as_json(state, "/conformance"))
    };

    page(
        state,
        "Conformance",
        &[Crumb::link("Home", "/"), Crumb::current("Conformance")],
        content,
    )
}

/// `/collections`: what this server can be queried for.
pub fn collections(state: &AppState) -> Markup {
    let collections = &state.catalog.collections;

    let content = html! {
        header.masthead {
            h1 { "Collections" }
            p.tagline {
                @match collections.len() {
                    1 => { "One collection is available to query." }
                    n => { (n) " collections are available to query." }
                }
            }
        }

        ul.resources {
            @for collection in collections.values() {
                li {
                    a href=(state.href(&format!("/collections/{}", collection.id))) {
                        (collection.title)
                    }
                    span.badge { (collection.id) }
                    p { (collection.description) }
                    p.facts {
                        (collection.parameters.len()) " parameters · "
                        (collection.x.len()) "×" (collection.y.len()) " grid · "
                        @if let Some(z) = collection.z.as_ref() {
                            (z.len()) " levels · "
                        }
                        (collection.t.len()) " time steps"
                    }
                }
            }
        }

        (as_json(state, "/collections"))
    };

    page(
        state,
        "Collections",
        &[Crumb::link("Home", "/"), Crumb::current("Collections")],
        content,
    )
}

/// How the parameter table is being used.
pub enum ParameterMode<'a> {
    /// Browsing a collection: each name links into the position form.
    Browse,
    /// Choosing parameters for a query: each name carries a checkbox.
    Select { selected: &'a [String] },
}

/// Every parameter of a collection, as one filterable table.
///
/// Browsing and choosing show the same facts — name, description, units, and
/// whether the variable varies with height — so they are one component with
/// one extra column, rather than two that drift apart. Which is also why the
/// checkbox list is a table now: units and the vertical axis are exactly what
/// you need in front of you while choosing, not only while reading.
fn parameter_table(state: &AppState, collection: &Collection, mode: ParameterMode<'_>) -> Markup {
    let selected: &[String] = match mode {
        ParameterMode::Select { selected } => selected,
        ParameterMode::Browse => &[],
    };
    let choosing = matches!(mode, ParameterMode::Select { .. });
    let level = collection.z_name.as_deref().unwrap_or("z");

    html! {
        (filter_control("parameters", "parameters", "temperature, wind, precipitation…"))
        div.table-scroll.catalogue {
            table #parameters {
                thead {
                    tr {
                        @if choosing { th.tick { span.visually-hidden { "Chosen" } } }
                        th { "Name" }
                        th { "Description" }
                        th { "Units" }
                        th { "Vertical" }
                    }
                }
                tbody {
                    @for parameter in collection.parameters.values() {
                        @let vertical = if parameter.uses_z {
                            format!("varies with {level}")
                        } else {
                            "surface".to_string()
                        };
                        // Lowercased once so the filter does no work per
                        // keystroke. The vertical wording is included, so
                        // typing "surface" narrows to surface variables —
                        // which is what the old grouping was for.
                        @let search = format!(
                            "{} {} {}",
                            parameter.name.to_lowercase(),
                            parameter.label.to_lowercase(),
                            vertical
                        );
                        @let id = format!("choose-{}", parameter.name);
                        tr data-search=(search) {
                            @if choosing {
                                td.tick {
                                    input type="checkbox" id=(id) name="parameter-name"
                                          value=(parameter.name)
                                          checked[selected.contains(&parameter.name)];
                                }
                            }
                            td {
                                @if choosing {
                                    // Clicking the name ticks the box.
                                    label for=(id) { code { (parameter.name) } }
                                } @else {
                                    a href=(state.href(&format!(
                                        "/collections/{}/position?parameter-name={}&f=html",
                                        collection.id, parameter.name
                                    ))) {
                                        code { (parameter.name) }
                                    }
                                }
                            }
                            td { (parameter.label) }
                            td {
                                @match &parameter.unit {
                                    Some(unit) => (unit),
                                    None => em { "unknown" },
                                }
                            }
                            td {
                                @if parameter.uses_z {
                                    (vertical)
                                } @else {
                                    span.muted { (vertical) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// `/collections/{id}`: extents, query endpoints, and every parameter.
pub fn collection(state: &AppState, collection: &Collection, host: Option<&str>) -> Markup {
    let base = format!("/collections/{}", collection.id);
    let [west, south, east, north] = collection.bbox();
    let (start, end) = collection.time_extent();
    let example_parameter = collection
        .parameters
        .keys()
        .find(|name| name.as_str() == "2m_temperature")
        .or_else(|| collection.parameters.keys().next());

    let content = html! {
        header.masthead {
            h1 { (collection.title) }
            p.tagline { (collection.description) }
            @if !collection.keywords.is_empty() {
                p {
                    @for keyword in &collection.keywords {
                        span.badge { (keyword) }
                    }
                }
            }
        }

        h2 { "Extent" }
        dl.extent {
            dt { "Spatial" }
            dd {
                (number(west)) ", " (number(south)) " to " (number(east)) ", " (number(north))
                " (CRS84)"
                br;
                small {
                    (collection.x.len()) " × " (collection.y.len()) " grid"
                    @if let Some(step) = collection.x.step() {
                        " at " (number(step.abs())) "°"
                    }
                }
            }

            dt { "Temporal" }
            dd {
                (format_time(start)) " to " (format_time(end))
                br;
                small { (collection.t.len()) " steps on the axis" }
            }

            @if let Some(z) = collection.z.as_ref() {
                dt { "Vertical" }
                dd {
                    (number(z.values[0])) " to " (number(z.values[z.len() - 1]))
                    @if let Some(units) = &collection.z_units { " " (units) }
                    br;
                    small {
                        (z.len()) " levels: "
                        @for (i, level) in z.values.iter().enumerate() {
                            @if i > 0 { ", " }
                            (number(*level))
                        }
                    }
                }
            }
        }

        h2 { "Queries" }
        ul.resources {
            @for (name, title) in QUERY_TYPES {
                li {
                    a href=(state.href(&format!("{base}/{name}"))) { (name) }
                    span.badge { "data" }
                    p { (title) }
                }
            }
        }
        p {
            "Responses are " (OUTPUT_FORMATS.join(" or ")) ". Every query needs "
            code { "parameter-name" } "; pick one from the table below."
        }
        @if let Some(parameter) = example_parameter {
            @let example = format!(
                "curl -G '{}' \\\n  \
                 --data-urlencode 'coords=POINT(-0.13 51.51)' \\\n  \
                 --data-urlencode 'parameter-name={}' \\\n  \
                 --data-urlencode 'datetime={}'",
                state.absolute_href(&format!("{base}/position"), host),
                parameter,
                format_time(end),
            );
            (command("example-query", &example))
        }

        h2 { "Parameters" }
        (parameter_table(state, collection, ParameterMode::Browse))

        (as_json(state, &base))
    };

    page(
        state,
        &collection.title,
        &[
            Crumb::link("Home", "/"),
            Crumb::link("Collections", "/collections"),
            Crumb::current(collection.title.clone()),
        ],
        content,
    )
}

/// How many result rows a page will show before it stops and points at the
/// data representation instead.
const MAX_RESULT_ROWS: usize = 500;

/// What came of running a query, if it was run at all.
pub enum Outcome<'a> {
    /// No geometry was given yet: the form is being shown for the first time.
    Blank,
    /// The query ran.
    Data {
        request: &'a DataRequest,
        result: &'a ResultSet,
        /// The request's own query string, for the "as data" links.
        query_string: String,
    },
    /// The query was rejected or found nothing.
    Failed(String),
}

/// `/collections/{id}/{query}`: the form, and whatever running it produced.
///
/// Form and result share one URL. The form submits to itself with `GET`, so a
/// result page's address is the API request that produced it — copyable,
/// bookmarkable, and one `f=` away from the data.
pub fn query_page(
    state: &AppState,
    collection: &Collection,
    kind: QueryKind,
    values: &Query,
    outcome: Outcome<'_>,
) -> Markup {
    let base = format!("/collections/{}", collection.id);
    let action = format!("{base}/{}", kind.path());
    let title = QUERY_TYPES
        .iter()
        .find(|(name, _)| *name == kind.path())
        .map(|(_, title)| *title)
        .unwrap_or_default();

    let content = html! {
        header.masthead {
            h1 { (collection.title) " · " (kind.path()) }
            p.tagline { (title) }
        }

        @if let Outcome::Failed(message) = &outcome {
            p.error role="alert" { (message) }
        }

        form method="get" action=(state.href(&action)) {
            // Keeps the browser on the HTML representation when the form is
            // submitted; `f=json` on the result links reaches the data.
            input type="hidden" name="f" value="html";

            (geometry_fields(kind, values))

            fieldset.parameters {
                legend {
                    "Parameters "
                    span.muted { "(" (collection.parameters.len()) " available)" }
                }
                @let selected = selected_parameters(values);
                (parameter_table(
                    state,
                    collection,
                    ParameterMode::Select {
                        selected: &selected,
                    },
                ))
            }

            @let (from, to) = datetime_bounds(values);
            @let (earliest, latest) = collection.time_extent();
            // Pickers stepped to the store's own cadence and bounded by its
            // extent, so an unusable instant cannot be chosen in the first
            // place.
            @let step = collection.t.step().map(|micros| (micros / 1_000_000).to_string());
            fieldset.parameters {
                legend { "Time " span.muted { "(UTC)" } }
                p.pair {
                    span {
                        label for="datetime-from" { "From" }
                        input #datetime-from type="datetime-local" name="datetime-from"
                              value=(from) min=(picker_time(earliest))
                              max=(picker_time(latest)) step=[step.as_deref()];
                    }
                    span {
                        label for="datetime-to" { "To" }
                        input #datetime-to type="datetime-local" name="datetime-to"
                              value=(to) min=(picker_time(earliest))
                              max=(picker_time(latest)) step=[step.as_deref()];
                    }
                }
                small.muted {
                    "Covers " (format_time(earliest)) " to " (format_time(latest)) ". "
                    "Leave both empty for the latest step, or one empty for an "
                    "open-ended interval."
                }
            }

            @if collection.z.is_some() {
                p {
                    label for="z" { "Vertical level" }
                    input #z type="text" name="z" value=(values.get("z").unwrap_or_default())
                          placeholder="850, or 500/1000, or all";
                    small.muted {
                        "Only applies to parameters that vary with "
                        (collection.z_name.as_deref().unwrap_or("z"))
                        ". Left empty, every level is returned."
                    }
                }
            }

            @if matches!(kind, QueryKind::Area | QueryKind::Cube) {
                p.pair {
                    span {
                        label for="resolution-x" { "Columns" }
                        input #resolution-x type="number" name="resolution-x" min="1"
                              value=(values.get("resolution-x").unwrap_or_default())
                              placeholder="all";
                    }
                    span {
                        label for="resolution-y" { "Rows" }
                        input #resolution-y type="number" name="resolution-y" min="1"
                              value=(values.get("resolution-y").unwrap_or_default())
                              placeholder="all";
                    }
                }
            }

            p { button type="submit" { "Run query" } }
        }

        @match &outcome {
            Outcome::Blank => {}
            Outcome::Failed(_) => {}
            Outcome::Data { request, result, query_string } => {
                (results(state, &action, request, result, query_string))
            }
        }

        p.alternate {
            "Back to " a href=(state.href(&base)) { (collection.title) }
        }
    };

    page(
        state,
        &format!("{} — {}", kind.path(), collection.title),
        &[
            Crumb::link("Home", "/"),
            Crumb::link("Collections", "/collections"),
            Crumb::link(collection.title.clone(), base),
            Crumb::current(kind.path()),
        ],
        content,
    )
}

/// The geometry input, which is the one field that differs per query type.
fn geometry_fields(kind: QueryKind, values: &Query) -> Markup {
    html! {
        @match kind {
            QueryKind::Cube => {
                p {
                    label for="bbox" { "Bounding box" }
                    input #bbox type="text" name="bbox" required
                          value=(values.get("bbox").unwrap_or_default())
                          placeholder="-0.5,51.0,0.5,52.0";
                    small.muted { "west, south, east, north in CRS84." }
                }
            }
            QueryKind::Area => {
                p {
                    label for="coords" { "Polygon" }
                    input #coords type="text" name="coords" required
                          value=(values.get("coords").unwrap_or_default())
                          placeholder="POLYGON((-1 51, 1 51, 1 52, -1 52, -1 51))";
                    small.muted { "A closed ring in CRS84. Holes may follow it." }
                }
            }
            QueryKind::Radius => {
                p {
                    label for="coords" { "Centre" }
                    input #coords type="text" name="coords" required
                          value=(values.get("coords").unwrap_or_default())
                          placeholder="POINT(-0.13 51.51)";
                }
                p.pair {
                    span {
                        label for="within" { "Within" }
                        input #within type="number" name="within" step="any" min="0" required
                              value=(values.get("within").unwrap_or_default())
                              placeholder="50";
                    }
                    span {
                        label for="within-units" { "Units" }
                        select #within-units name="within-units" {
                            @let chosen = values.get("within-units").unwrap_or("km");
                            @for unit in ["km", "m", "mi", "nmi", "ft"] {
                                option value=(unit) selected[unit == chosen] { (unit) }
                            }
                        }
                    }
                }
            }
            QueryKind::Position => {
                p {
                    label for="coords" { "Position" }
                    input #coords type="text" name="coords" required
                          value=(values.get("coords").unwrap_or_default())
                          placeholder="POINT(-0.13 51.51)";
                    small.muted {
                        "A POINT, or a MULTIPOINT for several at once. Each is "
                        "snapped to the nearest grid cell."
                    }
                }
            }
        }
    }
}

/// Parameter names already chosen, so the form comes back filled in. Resolved
/// exactly as the query resolves them, so what is ticked is what was asked for.
fn selected_parameters(values: &Query) -> Vec<String> {
    values.parameter_names().unwrap_or_default()
}

/// The result table, with only the axes that actually vary as columns.
fn results(
    state: &AppState,
    action: &str,
    request: &DataRequest,
    result: &ResultSet,
    query_string: &str,
) -> Markup {
    let names: Vec<&String> = result.parameter_names().collect();
    let rows: Vec<_> = result.rows().take(MAX_RESULT_ROWS + 1).collect();
    let truncated = rows.len() > MAX_RESULT_ROWS;
    let shown = &rows[..rows.len().min(MAX_RESULT_ROWS)];

    // A column earns its place only if its axis has more than one value; the
    // rest are stated once, above the table.
    let (times, levels) = (result.t.len() > 1, result.z.len() > 1);
    let (lats, lons) = (result.y.len() > 1, result.x.len() > 1);
    let level_name = request.collection.z_name.as_deref().unwrap_or("z");

    html! {
        h2 { "Result" }
        dl.extent {
            @if !lons && !lats {
                dt { "Position" }
                dd { (number(result.y[0])) ", " (number(result.x[0])) }
            }
            @if !times {
                dt { "Time" }
                dd { (format_time(result.t[0])) }
            }
            @if !levels && !result.z.is_empty() {
                dt { (level_name) }
                dd {
                    (number(result.z[0]))
                    @if let Some(units) = &request.collection.z_units { " " (units) }
                }
            }
            dt { "Values" }
            dd {
                (request.selection.value_count() * request.parameters.len())
                " across " (names.len())
                @if names.len() == 1 { " parameter" } @else { " parameters" }
            }
        }

        div.table-scroll {
            table {
                thead {
                    tr {
                        @if times { th { "Time" } }
                        @if levels { th { (level_name) } }
                        @if lats { th { "Latitude" } }
                        @if lons { th { "Longitude" } }
                        @for name in &names { th { (name) } }
                    }
                }
                tbody {
                    @for row in shown {
                        tr {
                            @if times { td { (format_time(row.t)) } }
                            @if levels { td { (row.z.map(number).unwrap_or_default()) } }
                            @if lats { td { (number(row.y)) } }
                            @if lons { td { (number(row.x)) } }
                            @for value in &row.values {
                                td {
                                    @match value {
                                        Some(v) => (format!("{v:.4}")),
                                        // Outside the polygon or radius, or
                                        // simply not recorded there.
                                        None => span.muted { "—" },
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        @if truncated {
            p.muted {
                "Showing the first " (MAX_RESULT_ROWS) " rows. Narrow the query, or "
                "take the whole result as data below."
            }
        }

        p.alternate {
            "This result as data: "
            a href=(state.href(&format!("{action}?{}", with_format(query_string, "CoverageJSON")))) {
                "CoverageJSON"
            }
            " · "
            a href=(state.href(&format!("{action}?{}", with_format(query_string, "GeoJSON")))) {
                "GeoJSON"
            }
        }
    }
}

/// Swap the `f` parameter of an already-encoded query string, and put the
/// form's paired time pickers back into EDR's single `datetime`.
///
/// The links out of a result page are the request in another format, so they
/// have to be a request any EDR client would accept — `datetime-from` is this
/// form's spelling, not the specification's.
fn with_format(query_string: &str, format: &str) -> String {
    let (mut from, mut to) = (None, None);
    let mut parts: Vec<&str> = Vec::new();
    for part in query_string.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        match key {
            "f" => {}
            "datetime-from" => from = Some(value),
            "datetime-to" => to = Some(value),
            _ => parts.push(part),
        }
    }

    // A submitted form sends both pickers whether or not they were touched, so
    // an empty one counts as absent — otherwise two untouched fields would
    // compose into `../..`, an interval bounding neither end.
    let (from, to) = (from.filter(|v| !v.is_empty()), to.filter(|v| !v.is_empty()));
    let composed = match (from, to) {
        // Only when the request did not already carry `datetime` itself, and
        // at least one picker was filled in.
        _ if parts.iter().any(|p| p.starts_with("datetime=")) => None,
        (None, None) => None,
        (from, to) => {
            fn open(value: Option<&str>) -> &str {
                match value {
                    Some(v) if !v.is_empty() => v,
                    _ => "..",
                }
            }
            Some(format!("datetime={}/{}", open(from), open(to)))
        }
    };
    if let Some(composed) = &composed {
        parts.push(composed);
    }

    let replacement = format!("f={format}");
    parts.push(&replacement);
    parts.join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_out_of_a_result_carry_the_specification_spelling() {
        // The form submits two pickers; an EDR client expects one `datetime`.
        let query = "coords=POINT(0%2045)&datetime-from=2024-01-01T00:00\
                     &datetime-to=2024-01-02T00:00&parameter-name=t&f=html";
        let link = with_format(query, "CoverageJSON");
        assert!(
            link.contains("datetime=2024-01-01T00:00/2024-01-02T00:00"),
            "{link}"
        );
        assert!(!link.contains("datetime-from"), "{link}");
        assert!(!link.contains("datetime-to"), "{link}");
        assert!(link.ends_with("f=CoverageJSON"), "{link}");
        assert!(link.contains("coords=POINT(0%2045)"), "{link}");
    }

    #[test]
    fn an_empty_picker_becomes_an_open_end() {
        let link = with_format("datetime-from=2024-01-01T00:00&datetime-to=", "GeoJSON");
        assert!(link.contains("datetime=2024-01-01T00:00/.."), "{link}");

        let link = with_format("datetime-from=&datetime-to=2024-01-02T00:00", "GeoJSON");
        assert!(link.contains("datetime=../2024-01-02T00:00"), "{link}");
    }

    #[test]
    fn untouched_pickers_add_no_datetime_at_all() {
        let link = with_format(
            "coords=POINT(0%2045)&datetime-from=&datetime-to=",
            "GeoJSON",
        );
        assert!(!link.contains("datetime"), "{link}");
    }

    #[test]
    fn an_explicit_datetime_is_left_exactly_as_it_came() {
        let query = "datetime=2024-01-01T00:00:00Z/..&datetime-from=&datetime-to=";
        let link = with_format(query, "CoverageJSON");
        assert!(link.contains("datetime=2024-01-01T00:00:00Z/.."), "{link}");
        // And is not composed over by the empty pickers beside it.
        assert_eq!(link.matches("datetime=").count(), 1, "{link}");
    }
}
