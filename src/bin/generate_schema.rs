//! Writes `schema.json` at the repo root.
//!
//! Mirrors the upstream `scripts/generate-schema.ts` shape so editors that
//! consume the schema get the same titles, descriptions, and defaults.

use std::path::PathBuf;

use serde_json::{json, Value};

fn build_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-7/schema#",
        "description": "Configuration schema for Wisetree - Git worktree management CLI",
        "type": "object",
        "properties": {
            "worktreeCopyPatterns": {
                "description": "File patterns to copy to new worktrees (glob patterns supported)",
                "default": [".env*", ".vscode/**"],
                "type": "array",
                "items": { "type": "string" }
            },
            "worktreeCopyIgnores": {
                "description": "File patterns to ignore when copying (glob patterns supported)",
                "default": [
                    "**/node_modules/**",
                    "**/dist/**",
                    "**/.git/**",
                    "**/Thumbs.db",
                    "**/.DS_Store"
                ],
                "type": "array",
                "items": { "type": "string" }
            },
            "worktreePathTemplate": {
                "description": "Template for worktree directory names. Variables: $BASE_PATH, $WORKTREE_PATH, $BRANCH_NAME, $SOURCE_BRANCH",
                "default": "$BASE_PATH.worktree",
                "type": "string"
            },
            "postCreateCmd": {
                "description": "Commands to run after creating a worktree. Variables: $BASE_PATH, $WORKTREE_PATH, $BRANCH_NAME, $SOURCE_BRANCH",
                "default": [],
                "type": "array",
                "items": { "type": "string" }
            },
            "worktreeLinkPatterns": {
                "description": "Directory patterns to symlink into new worktrees from the per-repository shared cache",
                "default": [],
                "type": "array",
                "items": { "type": "string" }
            },
            "worktreeLinkStrategy": {
                "description": "Strategy for handling missing source directories before linking",
                "default": "CreateEmpty",
                "type": "string",
                "enum": ["CreateEmpty", "SeedFromSource", "SeedIfPresent"]
            },
            "worktreeLinkCacheDir": {
                "description": "Optional override for the shared cache root. Variables: $BASE_PATH, $WORKTREE_PATH, $BRANCH_NAME, $SOURCE_BRANCH",
                "default": null,
                "type": ["string", "null"]
            },
            "terminalCommand": {
                "description": "Command to open terminal in new worktree directory (e.g., 'code $WORKTREE_PATH')",
                "default": "",
                "type": "string"
            },
            "deleteBranchWithWorktree": {
                "description": "Also delete the associated git branch when deleting a worktree",
                "default": false,
                "type": "boolean"
            }
        },
        "$id": "https://raw.githubusercontent.com/victorcorcos/wisetree/main/schema.json",
        "title": "Wisetree Configuration",
        "additionalProperties": false
    })
}

fn main() -> std::io::Result<()> {
    let schema = build_schema();
    let mut out = serde_json::to_string_pretty(&schema)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    out.push('\n');

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let path: PathBuf = PathBuf::from(manifest_dir).join("schema.json");
    std::fs::write(&path, out)?;
    println!("Generated JSON Schema at: {}", path.display());
    Ok(())
}
