//! llm.* — LLM response extraction bridge.
//!
//! Provides `llm.extract_json(text)` to extract JSON from LLM responses.
//! Uses `llm-extract` crate for fence stripping, bracket matching,
//! JSON repair, and parsing.

use mlua::prelude::*;

use super::json_to_lua;

pub fn register(lua: &Lua) -> LuaResult<()> {
    let llm_tbl = lua.create_table()?;

    // llm.extract_json(text) -> lua value (parsed JSON)
    llm_tbl.set(
        "extract_json",
        lua.create_function(|lua, text: String| {
            let value = llm_extract::extract_json(&text).map_err(LuaError::external)?;
            json_to_lua(lua, value)
        })?,
    )?;

    // llm.strip_fences(text) -> string (fence-stripped text)
    llm_tbl.set(
        "strip_fences",
        lua.create_function(|_, text: String| Ok(llm_extract::strip_fences(&text).to_string()))?,
    )?;

    lua.globals().set("llm", llm_tbl)?;
    Ok(())
}
