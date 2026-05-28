//! Schema generation for the generic resource API types.
//!
//! Produces deterministic, pretty-printed JSON Schema documents for the public
//! resource API types. Output is consumed by `mise run resource:schema:check`
//! and rendered in the operator docs via `JsonSchema.astro`.
//!
//! Determinism: `schemars::SchemaGenerator` emits keys in insertion order, but
//! `serde_json::to_string_pretty` walks `serde_json::Map`/`Value` in insertion
//! order too. To guarantee byte-identical output across runs and platforms we
//! round-trip the schema through a sorted JSON encoder. The result is committed
//! to `docs/engineering/public/schemas/` and verified in CI.
//!
//! Schemas generated here (filenames are stable):
//!   - `resource-envelope.schema.json`    — `Resource<JsonObject, JsonObject>`
//!   - `resource-metadata.schema.json`    — `ResourceMetadata`
//!   - `controller-status-map.schema.json` — `ControllerStatusMap`
//!   - `organization.schema.json`         — `Resource<OrganizationSpec, OrganizationStatus>`
//!   - `resource-definition.schema.json`  — `Resource<ResourceDefinitionSpec, ResourceDefinitionStatus>`
//!
//! `rise.toml` and backend settings schemas already have dedicated generators
//! (`backend rise-toml-schema`, `backend config-schema`); this module deliberately
//! does not duplicate them.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rise_resource_api::{
    ControllerStatusMap, OrganizationResource, Resource, ResourceDefinitionResource,
    ResourceMetadata,
};
use schemars::schema_for;

/// One schema artifact: a filename plus the rendered JSON Schema bytes.
pub struct SchemaFile {
    pub file_name: &'static str,
    pub contents: String,
}

/// Render `value` as deterministic, pretty-printed JSON with object keys sorted.
///
/// `schemars` emits keys in insertion order. Round-tripping through a
/// `BTreeMap`-backed walker gives us a stable byte-for-byte output regardless
/// of which fields were touched first during schema generation.
fn render_sorted_json(value: &serde_json::Value) -> String {
    let sorted = sort_keys(value);
    // `expect` is infallible by construction: `sorted` is a `serde_json::Value`
    // produced by `sort_keys`, which is always serializable. Trailing newline
    // is for editor conventions and clean `git diff`.
    format!(
        "{}\n",
        serde_json::to_string_pretty(&sorted).expect("sorted JSON value is always serializable")
    )
}

fn sort_keys(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: BTreeMap<String, serde_json::Value> = BTreeMap::new();
            for (k, v) in map {
                sorted.insert(k.clone(), sort_keys(v));
            }
            // serde_json's Value::Object preserves insertion order; inserting
            // BTreeMap entries in their (sorted) iteration order gives us the
            // sorted Map we want.
            let mut out = serde_json::Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k, v);
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sort_keys).collect())
        }
        other => other.clone(),
    }
}

/// Generate the resource-related schema files.
///
/// Stable iteration order: callers can pass the list straight to `write_to_dir`
/// and the resulting filesystem state is fully determined.
pub fn generate_schemas() -> Vec<SchemaFile> {
    vec![
        schema_file("resource-envelope.schema.json", schema_for!(Resource)),
        schema_file(
            "resource-metadata.schema.json",
            schema_for!(ResourceMetadata),
        ),
        schema_file(
            "controller-status-map.schema.json",
            schema_for!(ControllerStatusMap),
        ),
        schema_file(
            "organization.schema.json",
            schema_for!(OrganizationResource),
        ),
        schema_file(
            "resource-definition.schema.json",
            schema_for!(ResourceDefinitionResource),
        ),
    ]
}

/// Build one `SchemaFile` from a `schemars`-produced `Schema`.
///
/// `serde_json::to_value` on a `schemars::Schema` is infallible by construction
/// — the schema is built from a `serde_json::Value` internally and has no
/// non-serializable shapes (no non-finite floats, no non-string map keys).
/// The `.expect` documents the invariant rather than papering over a real
/// fallible operation.
fn schema_file(file_name: &'static str, schema: schemars::Schema) -> SchemaFile {
    let value = serde_json::to_value(schema)
        .expect("schemars-produced Schema is always serializable to serde_json::Value");
    SchemaFile {
        file_name,
        contents: render_sorted_json(&value),
    }
}

/// Write each generated schema into `out_dir`. Creates `out_dir` if missing.
///
/// Returns the path of each file written (`out_dir.join(<file_name>)`,
/// resolved relative to the caller's working directory if `out_dir` itself
/// is relative). Output is byte-identical across runs, so this is safe to
/// call repeatedly (idempotent).
pub fn write_to_dir(out_dir: &Path) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(out_dir)
        .with_context(|| format!("creating output directory {}", out_dir.display()))?;

    let files = generate_schemas();
    let mut written = Vec::with_capacity(files.len());
    for file in files {
        let path = out_dir.join(file.file_name);
        write_if_changed(&path, file.contents.as_bytes())
            .with_context(|| format!("writing schema {}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

/// Write `bytes` to `path` only if the file is missing or its current contents
/// differ. Avoids spurious `mtime` churn that would cause `cargo` rebuilds in
/// downstream tools that watch the docs directory.
fn write_if_changed(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Ok(existing) = fs::read(path) {
        if existing == bytes {
            return Ok(());
        }
    }
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    Ok(())
}

/// Pretty-print each generated schema to `stdout`, separated by a marker line.
///
/// Useful for piping into other tooling or just eyeballing the output.
pub fn print_to_stdout() -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for file in generate_schemas() {
        writeln!(out, "// === {} ===", file.file_name)?;
        out.write_all(file.contents.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full output set must be byte-identical across two consecutive
    /// invocations. This is the property `mise run resource:schema:check`
    /// relies on.
    #[test]
    fn schemas_are_deterministic_across_runs() {
        let first: Vec<(String, String)> = generate_schemas()
            .into_iter()
            .map(|f| (f.file_name.to_string(), f.contents))
            .collect();
        let second: Vec<(String, String)> = generate_schemas()
            .into_iter()
            .map(|f| (f.file_name.to_string(), f.contents))
            .collect();
        assert_eq!(first, second);
    }

    /// Each file ends with a trailing newline so editors don't add one on save
    /// and trip up `git diff` in the check task.
    #[test]
    fn all_files_end_with_newline() {
        for file in generate_schemas() {
            assert!(
                file.contents.ends_with('\n'),
                "{} missing trailing newline",
                file.file_name
            );
        }
    }

    /// Object keys must be in sorted order at every level. A regression in the
    /// sort would surface here long before CI noticed the diff in the
    /// committed schema files.
    #[test]
    fn object_keys_are_sorted() {
        for file in generate_schemas() {
            let value: serde_json::Value =
                serde_json::from_str(&file.contents).expect("parse schema JSON");
            assert_keys_sorted(&value, file.file_name, "");
        }
    }

    fn assert_keys_sorted(value: &serde_json::Value, file: &str, path: &str) {
        if let serde_json::Value::Object(map) = value {
            let keys: Vec<&String> = map.keys().collect();
            for w in keys.windows(2) {
                assert!(
                    w[0] <= w[1],
                    "{file}: unsorted keys at '{path}': '{}' before '{}'",
                    w[0],
                    w[1]
                );
            }
            for (k, v) in map {
                let child_path = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                assert_keys_sorted(v, file, &child_path);
            }
        } else if let serde_json::Value::Array(items) = value {
            for (i, v) in items.iter().enumerate() {
                assert_keys_sorted(v, file, &format!("{path}[{i}]"));
            }
        }
    }

    /// Sanity-check that we emit the schemas the docs page expects to link.
    #[test]
    fn emits_expected_filenames() {
        let names: Vec<&str> = generate_schemas().iter().map(|f| f.file_name).collect();
        assert_eq!(
            names,
            vec![
                "resource-envelope.schema.json",
                "resource-metadata.schema.json",
                "controller-status-map.schema.json",
                "organization.schema.json",
                "resource-definition.schema.json",
            ]
        );
    }
}
