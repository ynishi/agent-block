//! `std.kv` — JSON file-backed key-value store for Lua scripts.
//!
//! Each namespace maps to a single JSON file under `{base_dir}/kv/{ns}.json`.
//! Writes are atomic (write to `.tmp` then rename).

use std::path::{Path, PathBuf};

use mlua::prelude::*;

use crate::host::HostContext;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the base directory for kv storage.
///
/// Priority: `$AGENT_BLOCK_HOME` → `$HOME/.agent-block`
fn base_dir() -> Result<PathBuf, String> {
    if let Some(v) = std::env::var_os("AGENT_BLOCK_HOME") {
        return Ok(PathBuf::from(v));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME env var not set".to_string())?;
    Ok(PathBuf::from(home).join(".agent-block"))
}

/// Validate a namespace string.
///
/// Rejects: empty, contains `/`, `\`, `..`, or `\0`.
fn validate_ns(ns: &str) -> Result<(), String> {
    if ns.is_empty() {
        return Err(format!("Invalid namespace: '{ns}'"));
    }
    if ns.contains('/') || ns.contains('\\') || ns.contains('\0') || ns.contains("..") {
        return Err(format!("Invalid namespace: '{ns}'"));
    }
    Ok(())
}

/// Return the path to the JSON file for a given namespace.
fn ns_path(ns: &str) -> Result<PathBuf, String> {
    validate_ns(ns)?;
    let base = base_dir()?;
    Ok(base.join("kv").join(format!("{ns}.json")))
}

/// Load the JSON map from disk. Returns an empty map if the file does not exist.
fn load_map(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let val: serde_json::Value = serde_json::from_str(&content)
                .map_err(|e| format!("Failed to parse kv file {}: {e}", path.display()))?;
            match val {
                serde_json::Value::Object(map) => Ok(map),
                other => Err(format!(
                    "Expected JSON object in kv file {}, got {}",
                    path.display(),
                    other
                )),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::Map::new()),
        Err(e) => Err(format!("Failed to read kv file {}: {e}", path.display())),
    }
}

/// Atomically write a JSON map to disk.
///
/// Strategy: write to `{path}.tmp`, then rename to `{path}`.
fn save_map(path: &Path, map: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create kv directory {}: {e}", parent.display()))?;
    }

    let content = serde_json::to_string_pretty(&serde_json::Value::Object(map.clone()))
        .map_err(|e| format!("Failed to serialize kv map: {e}"))?;

    // Build tmp path: append ".tmp" to the full path string.
    let mut tmp_path = path.as_os_str().to_owned();
    tmp_path.push(".tmp");
    let tmp_path = PathBuf::from(tmp_path);

    std::fs::write(&tmp_path, &content)
        .map_err(|e| format!("Failed to write tmp kv file {}: {e}", tmp_path.display()))?;

    std::fs::rename(&tmp_path, path).map_err(|e| {
        // Best-effort cleanup of the tmp file on rename failure.
        let _ = std::fs::remove_file(&tmp_path);
        format!(
            "Failed to rename {} -> {}: {e}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Lua operations
// ---------------------------------------------------------------------------

/// `std.kv.get(ns, key) -> value | nil`
fn kv_get(lua: &Lua, ns: &str, key: &str) -> LuaResult<LuaValue> {
    let path = ns_path(ns).map_err(LuaError::external)?;
    let map = load_map(&path).map_err(LuaError::external)?;
    match map.get(key) {
        Some(val) => super::json_to_lua(lua, val.clone()),
        None => Ok(LuaValue::Nil),
    }
}

/// `std.kv.set(ns, key, value) -> void`
fn kv_set(lua: &Lua, ns: &str, key: &str, value: LuaValue) -> LuaResult<()> {
    let path = ns_path(ns).map_err(LuaError::external)?;
    let mut map = load_map(&path).map_err(LuaError::external)?;
    let json_val = super::lua_to_json(lua, value)?;
    map.insert(key.to_string(), json_val);
    save_map(&path, &map).map_err(LuaError::external)
}

/// `std.kv.delete(ns, key) -> bool`
fn kv_delete(ns: &str, key: &str) -> LuaResult<bool> {
    let path = ns_path(ns).map_err(LuaError::external)?;
    let mut map = load_map(&path).map_err(LuaError::external)?;
    if map.remove(key).is_some() {
        save_map(&path, &map).map_err(LuaError::external)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// `std.kv.list(ns, prefix?) -> string[]`
fn kv_list(lua: &Lua, ns: &str, prefix: Option<String>) -> LuaResult<LuaTable> {
    let path = ns_path(ns).map_err(LuaError::external)?;
    let map = load_map(&path).map_err(LuaError::external)?;

    let tbl = lua.create_table()?;
    let mut idx = 1usize;
    for key in map.keys() {
        let include = prefix.as_deref().map_or(true, |p| key.starts_with(p));
        if include {
            tbl.set(idx, key.as_str())?;
            idx += 1;
        }
    }
    Ok(tbl)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(lua: &Lua, _ctx: &HostContext) -> LuaResult<()> {
    let kv_tbl = lua.create_table()?;

    kv_tbl.set(
        "get",
        lua.create_function(|lua, (ns, key): (String, String)| kv_get(lua, &ns, &key))?,
    )?;

    kv_tbl.set(
        "set",
        lua.create_function(|lua, (ns, key, value): (String, String, LuaValue)| {
            kv_set(lua, &ns, &key, value)
        })?,
    )?;

    kv_tbl.set(
        "delete",
        lua.create_function(|_, (ns, key): (String, String)| kv_delete(&ns, &key))?,
    )?;

    kv_tbl.set(
        "list",
        lua.create_function(|lua, (ns, prefix): (String, Option<String>)| {
            kv_list(lua, &ns, prefix)
        })?,
    )?;

    let std_ns: LuaTable = lua.globals().get("std")?;
    std_ns.set("kv", kv_tbl)?;

    Ok(())
}
