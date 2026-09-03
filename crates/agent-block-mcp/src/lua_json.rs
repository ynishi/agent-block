//! Lua ↔ JSON value bridge.
//!
//! Moved from `src/bridge/mod.rs` during the 4-crate split so that the MCP
//! handler can reach these conversions without depending on `agent-block-core`.
//! `agent-block-core::bridge` re-exports them for the other bridge modules
//! (llm / mesh / mcp.lua), preserving the historical `crate::bridge::*` API.
//!
//! # The empty table, and how a value says which kind it is
//!
//! Lua has one table type, so `{}` is an empty array and an empty object
//! at once and nothing in the value itself tells the two apart.  An
//! untagged empty table is read here as an object, which is what this
//! bridge has always done — but a producer that means the other one has a
//! way to say so, because a boundary that cannot express `[]` pushes the
//! shapes that need one into inventing content that was never there.
//!
//! A table declares itself an array by carrying a metatable that says so:
//!
//! * `__jsontype = "array"`, the tag Lua writes for itself
//!   (`setmetatable({}, { __jsontype = "array" })`) and the convention the
//!   Lua JSON libraries already use, or
//! * mlua's own array metatable, so values its serde bridge produced are
//!   read the same way rather than by a second rule.
//!
//! A tagged table is encoded from its sequence part, which is mlua's rule
//! for the same tag.  [`json_to_lua`] tags the empty arrays it builds, so
//! `[]` comes back as `[]` instead of turning into `{}` on the way home;
//! a non-empty array needs no tag — a sequence is already unambiguous —
//! and is left as it was, so no existing value changes shape.

use mlua::prelude::*;

/// Metatable field a table carries to declare itself a JSON array.
pub const ARRAY_METAFIELD: &str = "__jsontype";
/// The value of [`ARRAY_METAFIELD`] that means "array".
pub const ARRAY_METAFIELD_VALUE: &str = "array";
/// Where the shared array metatable is parked, once per Lua state.
const ARRAY_METATABLE_REGISTRY_KEY: &str = "agent_block.lua_json.array_metatable";

/// The metatable that marks a Lua table as a JSON array.
///
/// One table per Lua state, so every empty array [`json_to_lua`] builds
/// shares it and a caller can compare against it.  Plain (`__metatable` is
/// not set): the tag is a fact about the value, not a lock on it, and Lua
/// code that wants to give the table a metatable of its own still can.
pub fn array_metatable(lua: &Lua) -> LuaResult<LuaTable> {
    if let Ok(LuaValue::Table(existing)) =
        lua.named_registry_value::<LuaValue>(ARRAY_METATABLE_REGISTRY_KEY)
    {
        return Ok(existing);
    }
    let mt = lua.create_table()?;
    mt.raw_set(ARRAY_METAFIELD, ARRAY_METAFIELD_VALUE)?;
    lua.set_named_registry_value(ARRAY_METATABLE_REGISTRY_KEY, &mt)?;
    Ok(mt)
}

/// Whether `table` declares itself an array.
///
/// Read raw: the tag is a plain field of the metatable, and an `__index`
/// on it is the table's own business rather than a source of array-ness.
fn is_tagged_array(lua: &Lua, table: &LuaTable) -> LuaResult<bool> {
    let Some(mt) = table.metatable() else {
        return Ok(false);
    };
    if let LuaValue::String(kind) = mt.raw_get::<LuaValue>(ARRAY_METAFIELD)? {
        if &*kind.to_str()? == ARRAY_METAFIELD_VALUE {
            return Ok(true);
        }
    }
    // The same question asked of a value mlua's serde bridge built.
    Ok(mt == lua.array_metatable())
}

/// Convert a Lua value to a serde_json::Value.
///
/// Round-trips with `json_to_lua` and `std.json.encode` (mlua-batteries).
/// Lua `nil` maps to JSON `null`.  Unsupported types (functions, userdata
/// other than `null`) yield an error so that callers do not silently emit
/// malformed JSON.
///
/// An empty table becomes `{}` unless it is tagged as an array (see the
/// module docs), in which case it becomes `[]`.
pub fn lua_to_json(lua: &Lua, val: LuaValue) -> LuaResult<serde_json::Value> {
    lua_to_json_inner(lua, &val, 0)
}

fn lua_to_json_inner(lua: &Lua, val: &LuaValue, depth: usize) -> LuaResult<serde_json::Value> {
    const MAX_DEPTH: usize = 128;
    if depth > MAX_DEPTH {
        return Err(LuaError::external(format!(
            "Lua table nesting too deep for JSON (limit: {MAX_DEPTH})"
        )));
    }
    match val {
        LuaValue::Nil => Ok(serde_json::Value::Null),
        // mlua serde uses LightUserData(null_ptr) for JSON null.  Treat it
        // the same as Nil so values produced by `json_to_lua` round-trip.
        LuaValue::LightUserData(u) if u.0.is_null() => Ok(serde_json::Value::Null),
        LuaValue::Boolean(b) => Ok(serde_json::Value::Bool(*b)),
        LuaValue::Integer(i) => Ok(serde_json::Value::Number((*i).into())),
        LuaValue::Number(n) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .ok_or_else(|| LuaError::external(format!("cannot convert {n} to JSON number"))),
        LuaValue::String(s) => Ok(serde_json::Value::String(s.to_str()?.to_string())),
        LuaValue::Table(t) => {
            let len = t.raw_len();
            if len > 0 || is_tagged_array(lua, t)? {
                let mut arr = Vec::with_capacity(len);
                for i in 1..=len {
                    let v: LuaValue = t.raw_get(i)?;
                    arr.push(lua_to_json_inner(lua, &v, depth + 1)?);
                }
                Ok(serde_json::Value::Array(arr))
            } else {
                let mut map = serde_json::Map::new();
                for pair in t.clone().pairs::<LuaValue, LuaValue>() {
                    let (k, v) = pair?;
                    let key = match k {
                        LuaValue::String(s) => s.to_str()?.to_string(),
                        LuaValue::Integer(i) => i.to_string(),
                        LuaValue::Number(n) => n.to_string(),
                        other => {
                            return Err(LuaError::external(format!(
                                "unsupported table key type for JSON: {}",
                                other.type_name()
                            )));
                        }
                    };
                    map.insert(key, lua_to_json_inner(lua, &v, depth + 1)?);
                }
                Ok(serde_json::Value::Object(map))
            }
        }
        other => Err(LuaError::external(format!(
            "unsupported type for JSON conversion: {}",
            other.type_name()
        ))),
    }
}

/// Convert a serde_json::Value to a Lua value.
///
/// JSON `null` maps to the `LightUserData(null_ptr)` sentinel
/// (`mlua::Value::NULL`), which is the same representation `lua_to_json`
/// accepts on the way out — so the round-trip is symmetric.  Using the
/// sentinel rather than Lua `nil` means JSON `null` values survive being
/// placed into Lua tables (tables cannot hold `nil`), so SQL NULL columns
/// and MCP/LLM JSON payloads do not lose the distinction between "null"
/// and "absent".  Agents can compare a value against the exposed
/// `std.sql.null` constant to detect it.
///
/// Note: this differs from mlua-batteries' `std.json.decode`, which keeps
/// the Lua-idiomatic "null → nil" lowering for `json.decode` itself.  Our
/// bridge paths (sql / kv / mcp / mesh / llm) prefer round-trip fidelity.
///
/// An empty array comes back tagged (see the module docs) so that it is
/// still an array on the way back through [`lua_to_json`]; every other
/// value is built exactly as it was before the tag existed.
pub fn json_to_lua(lua: &Lua, val: serde_json::Value) -> LuaResult<LuaValue> {
    json_to_lua_inner(lua, &val, 0)
}

fn json_to_lua_inner(lua: &Lua, val: &serde_json::Value, depth: usize) -> LuaResult<LuaValue> {
    const MAX_DEPTH: usize = 128;
    if depth > MAX_DEPTH {
        return Err(LuaError::external(format!(
            "JSON nesting too deep (limit: {MAX_DEPTH})"
        )));
    }
    match val {
        serde_json::Value::Null => Ok(LuaValue::NULL),
        serde_json::Value::Bool(b) => Ok(LuaValue::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(LuaValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(LuaValue::Number(f))
            } else {
                Err(LuaError::external(format!(
                    "JSON number {n} is not representable as i64 or f64"
                )))
            }
        }
        serde_json::Value::String(s) => lua.create_string(s).map(LuaValue::String),
        serde_json::Value::Array(arr) => {
            let table = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                table.set(i + 1, json_to_lua_inner(lua, v, depth + 1)?)?;
            }
            // Only the empty one needs saying: a table with a sequence in
            // it already reads as an array, and tagging it would change a
            // value that was fine as it was.
            if arr.is_empty() {
                table.set_metatable(Some(array_metatable(lua)?))?;
            }
            Ok(LuaValue::Table(table))
        }
        serde_json::Value::Object(map) => {
            let table = lua.create_table()?;
            for (k, v) in map {
                table.set(k.as_str(), json_to_lua_inner(lua, v, depth + 1)?)?;
            }
            Ok(LuaValue::Table(table))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `value` after a trip into Lua and back.
    fn round_trip(lua: &Lua, value: serde_json::Value) -> serde_json::Value {
        let in_lua = json_to_lua(lua, value.clone()).expect("into Lua");
        lua_to_json(lua, in_lua).unwrap_or_else(|e| panic!("{value}: {e}"))
    }

    /// The value a Lua expression evaluates to, as JSON.
    fn encode(lua: &Lua, chunk: &str) -> serde_json::Value {
        let value: LuaValue = lua
            .load(chunk)
            .eval()
            .unwrap_or_else(|e| panic!("{chunk}: {e}"));
        lua_to_json(lua, value).unwrap_or_else(|e| panic!("{chunk}: {e}"))
    }

    /// The two empty shapes are the ones a single Lua table cannot tell
    /// apart on its own, so they are what the tag is for: each comes back
    /// as itself, including where they are nested inside one another.
    #[test]
    fn the_two_empty_shapes_survive_a_round_trip() {
        let lua = Lua::new();
        for value in [
            json!([]),
            json!({}),
            json!({ "content": [], "usage": {} }),
            json!([[], {}, [[]]]),
            json!({ "a": { "b": [] }, "c": [{}] }),
            json!({ "content": [{ "type": "text", "text": "hi" }] }),
        ] {
            assert_eq!(round_trip(&lua, value.clone()), value, "{value}");
        }
    }

    /// The tag is the only thing that separates them: an untagged empty
    /// table is still an object, which is what every caller that never
    /// heard of the tag keeps getting.
    #[test]
    fn an_untagged_empty_table_is_still_an_object() {
        let lua = Lua::new();
        assert_eq!(encode(&lua, "return {}"), json!({}));
        assert_eq!(encode(&lua, "return { a = {} }"), json!({ "a": {} }));
    }

    /// A table Lua tagged for itself is read as an array, and so is one
    /// mlua's own serde bridge tagged — two ways of saying it, one answer.
    #[test]
    fn a_tagged_empty_table_is_an_array() {
        let lua = Lua::new();
        assert_eq!(
            encode(&lua, r#"return setmetatable({}, { __jsontype = "array" })"#),
            json!([])
        );

        lua.globals()
            .set("mlua_array_mt", lua.array_metatable())
            .expect("expose mlua's array metatable");
        assert_eq!(
            encode(&lua, "return setmetatable({}, mlua_array_mt)"),
            json!([])
        );

        // A tag that says something else is not this tag.
        assert_eq!(
            encode(
                &lua,
                r#"return setmetatable({}, { __jsontype = "object" })"#
            ),
            json!({})
        );
        assert_eq!(
            encode(&lua, r#"return setmetatable({}, { __index = {} })"#),
            json!({})
        );
    }

    /// The tag names the shape, it does not lock the table: what comes back
    /// from JSON can still be given a metatable of its own, and reading its
    /// metatable answers rather than refusing.
    #[test]
    fn the_tag_leaves_the_table_usable() {
        let lua = Lua::new();
        lua.globals()
            .set("empty", json_to_lua(&lua, json!([])).expect("into Lua"))
            .expect("set");
        lua.load(
            r#"
            assert(#empty == 0, "a tagged empty array is still empty")
            assert(next(empty) == nil, "the tag must not put a field in the table")
            assert(getmetatable(empty).__jsontype == "array", "the tag must be readable")
            assert(pcall(setmetatable, empty, {}), "the tag must not protect the table")
        "#,
        )
        .exec()
        .expect("tagged empty array chunk");
    }

    /// One metatable per state, so every empty array a run produces carries
    /// the same tag and a caller can compare against it.
    #[test]
    fn every_empty_array_shares_one_metatable() {
        let lua = Lua::new();
        let first = json_to_lua(&lua, json!([])).expect("into Lua");
        let second = json_to_lua(&lua, json!({ "a": [] })).expect("into Lua");
        lua.globals().set("first", first).expect("set");
        lua.globals().set("second", second).expect("set");
        lua.load(
            r#"
            assert(getmetatable(first) == getmetatable(second.a), "two array tags in one state")
            assert(getmetatable(first) ~= nil)
        "#,
        )
        .exec()
        .expect("shared metatable chunk");
    }

    /// Whatever the tag says, a table with a sequence in it is an array,
    /// and a tagged table is encoded from that sequence — mlua's rule for
    /// the same tag, so one table does not mean two things.
    #[test]
    fn a_sequence_is_an_array_tag_or_no_tag() {
        let lua = Lua::new();
        assert_eq!(encode(&lua, "return { 1, 2, 3 }"), json!([1, 2, 3]));
        assert_eq!(
            encode(
                &lua,
                r#"return setmetatable({ 1, 2 }, { __jsontype = "array" })"#
            ),
            json!([1, 2])
        );
        assert_eq!(
            encode(
                &lua,
                r#"return setmetatable({ a = 5 }, { __jsontype = "array" })"#
            ),
            json!([]),
            "a tagged table is its sequence part"
        );
    }
}
