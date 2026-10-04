//! Generate the configuration schema from the Rust configuration types.
use std::path::PathBuf;
fn main() -> std::io::Result<()> {
    let mut schema = schemars::schema_for!(wisetree::config::schema::WorktreeConfig);
    schema.meta_schema = Some("http://json-schema.org/draft-07/schema#".to_string());
    let mut output = serde_json::to_string_pretty(&schema).map_err(std::io::Error::other)?;
    output.push('\n');
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schema.json");
    std::fs::write(&path, output)?;
    println!("Generated JSON Schema at: {}", path.display());
    Ok(())
}
