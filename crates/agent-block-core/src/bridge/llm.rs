//! `llm.*` — extraction helpers for text that came back from a model.
//!
//! Model output is prose with structure buried in it: fences the model may
//! forget to close, `<think>` blocks from reasoning models, JSON with a
//! trailing comma. Every block that reads model output needs the same
//! salvage step, and writing it in Lua means writing a parser in Lua
//! patterns — which is how `compile_loop` ended up with a three-branch
//! fence matcher that returns the raw response when the closing fence is
//! missing.
//!
//! The `llm-extract` crate does this work (fence stripping, think-block
//! removal, bracket matching, JSON repair); this bridge exposes it so the
//! Lua side calls it instead of reimplementing it.

use mlua::prelude::*;

use super::json_to_lua;

/// Content of the first fenced block whose info string is exactly `lang`.
///
/// `strip_fences` takes the *first* fence whatever its tag, which is the
/// wrong choice when the model narrates in a ```text block before emitting
/// the ```rust one. A tag match picks the block that was asked for.
///
/// An unclosed block runs to the end of the input rather than failing: a
/// truncated response still carries the code written so far, and returning
/// the whole response instead is how think-text ends up written to a file.
fn fenced_block_tagged(text: &str, lang: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let Some(info) = lines[i].trim_start().strip_prefix("```") else {
            i += 1;
            continue;
        };
        let mut end = i + 1;
        while end < lines.len() && !lines[end].trim_start().starts_with("```") {
            end += 1;
        }
        if info.trim().eq_ignore_ascii_case(lang) {
            return Some(lines[i + 1..end].join("\n"));
        }
        // Past the closing fence: a fence inside a block does not open one.
        i = end + 1;
    }
    None
}

/// Pull the code out of a model response.
///
/// Think blocks are removed first, so a fence the model wrote *while
/// reasoning* cannot be mistaken for the answer. Then the `lang`-tagged
/// block if there is one, then any fenced block, then the text as-is.
fn extract_code(text: &str, lang: Option<&str>) -> String {
    let stripped = llm_extract::strip_think_blocks(text);
    let body = stripped.as_ref();
    if let Some(lang) = lang {
        if let Some(code) = fenced_block_tagged(body, lang) {
            return code;
        }
    }
    llm_extract::strip_fences(body).to_string()
}

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

    // llm.strip_think_blocks(text) -> string (text with <think>...</think> removed)
    llm_tbl.set(
        "strip_think_blocks",
        lua.create_function(|_, text: String| {
            Ok(llm_extract::strip_think_blocks(&text).into_owned())
        })?,
    )?;

    // llm.extract_code(text, lang?) -> string
    llm_tbl.set(
        "extract_code",
        lua.create_function(|_, (text, lang): (String, Option<String>)| {
            Ok(extract_code(&text, lang.as_deref()))
        })?,
    )?;

    lua.globals().set("llm", llm_tbl)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::extract_code;

    #[test]
    fn takes_the_lang_tagged_block() {
        assert_eq!(
            extract_code("```lua\nprint('x')\n```", Some("lua")),
            "print('x')"
        );
    }

    #[test]
    fn falls_back_to_any_fence_when_the_lang_fence_is_absent() {
        assert_eq!(extract_code("```python\nx = 1\n```", Some("lua")), "x = 1");
    }

    #[test]
    fn returns_the_text_when_there_is_no_fence() {
        assert_eq!(
            extract_code("no fences here", Some("lua")),
            "no fences here"
        );
    }

    #[test]
    fn prefers_the_lang_tagged_block_over_an_earlier_one() {
        let text = "```text\nhere is the fix\n```\n```lua\nprint('x')\n```";
        assert_eq!(extract_code(text, Some("lua")), "print('x')");
    }

    #[test]
    fn keeps_a_truncated_block_instead_of_the_whole_response() {
        // No closing fence: the Lua pattern matcher used to fail every branch
        // and hand back the entire response as if it were code.
        assert_eq!(
            extract_code("here:\n```lua\nprint('x')", Some("lua")),
            "print('x')"
        );
    }

    #[test]
    fn ignores_a_fence_written_inside_a_think_block() {
        let text = "<think>maybe ```lua\nwrong()\n``` ?</think>\n```lua\nright()\n```";
        assert_eq!(extract_code(text, Some("lua")), "right()");
    }

    #[test]
    fn multi_line_bodies_survive_verbatim() {
        assert_eq!(
            extract_code("```lua\nlocal a = 1\nreturn a\n```", Some("lua")),
            "local a = 1\nreturn a"
        );
    }
}
