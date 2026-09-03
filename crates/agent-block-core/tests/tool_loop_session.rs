//! `tool_loop.run` driving a kernel session (`opts.session`).
//!
//! # Why these live here and not with the other tool_loop specs
//!
//! The Lua spec fixtures run in a plain VM with no bridges registered — they
//! stub `std` / `http` / `log` themselves — so `knl` is not reachable from
//! there. A fake session written in Lua would test the loop against a mirror of
//! the kernel's rules rather than the rules, and the part most worth pinning is
//! exactly the part a mirror would drift from: which fields the reserved kinds
//! require, and that an empty Lua table reaches the kernel as a mapping rather
//! than an array. So the session cases run against the real bridge here, and
//! the fixture keeps the cases that need no kernel (the result contract, and
//! that a run without a session is untouched).
//!
//! The transport is stubbed at the two ends the way the fixture does it: the
//! request body encodes to a sentinel and the response body decodes back to
//! whatever the test queued. What is under test is which facts the loop records
//! and when, not the wire format.

use mlua::prelude::*;

/// The Lua modules `require("tool_loop")` pulls in, under the names the host
/// registers them as.
const LIBS: &[(&str, &str)] = &[
    (
        "tool_loop",
        include_str!("../blocks/lib/tool_loop/init.lua"),
    ),
    (
        "llm_proto",
        include_str!("../blocks/lib/llm_proto/init.lua"),
    ),
    (
        "llm_proto.openai",
        include_str!("../blocks/lib/llm_proto/openai.lua"),
    ),
    (
        "llm_proto.anthropic",
        include_str!("../blocks/lib/llm_proto/anthropic.lua"),
    ),
    ("lshape", include_str!("../blocks/lib/lshape/init.lua")),
    ("lshape.t", include_str!("../blocks/lib/lshape/t.lua")),
    (
        "lshape.check",
        include_str!("../blocks/lib/lshape/check.lua"),
    ),
    (
        "lshape.reflect",
        include_str!("../blocks/lib/lshape/reflect.lua"),
    ),
    (
        "lshape.luacats",
        include_str!("../blocks/lib/lshape/luacats.lua"),
    ),
];

/// Host stubs plus the fixtures every case builds on.
const PRELUDE: &str = r#"
    local RESPONSE_BODY = "<canned-response-body>"
    local queue = {}

    -- The responses the stubbed transport will hand back, in order.
    function queue_responses(...)
        queue = { ... }
    end

    -- How many queued responses were never asked for: the way a case shows
    -- that the loop stopped instead of calling the model again.
    function queued_left()
        return #queue
    end

    log = { warn = function() end, info = function() end, debug = function() end, error = function() end }

    std = {
        env = {
            get = function() return nil end,
            get_or = function(_, default) return default end,
            agent_id = function() return nil end,
        },
        json = {
            encode = function() return "<encoded-request-body>" end,
            decode = function(s)
                assert(s == RESPONSE_BODY, "unexpected body to decode: " .. tostring(s))
                local response = table.remove(queue, 1)
                assert(response, "the loop asked for a response the test did not queue")
                return response
            end,
        },
        time = { now = function() return 0 end },
        task = { sleep = function() end },
    }

    http = {
        request = function()
            return { status = 200, body = RESPONSE_BODY, headers = {} }
        end,
    }

    function text_response(text, usage)
        return {
            id = "msg_text",
            role = "assistant",
            content = { { type = "text", text = text } },
            stop_reason = "end_turn",
            usage = usage or { input_tokens = 3, output_tokens = 4 },
        }
    end

    function tool_response(name, input, usage)
        return {
            id = "msg_tool",
            role = "assistant",
            content = {
                { type = "tool_use", id = "call_1", name = name, input = input or { path = "a.txt" } },
            },
            stop_reason = "tool_use",
            usage = usage or {
                input_tokens = 10,
                output_tokens = 5,
                output_tokens_details = { thinking_tokens = 2 },
            },
        }
    end

    echo_tool = {
        name = "echo",
        description = "echo the path it is given",
        input_schema = { type = "object" },
        handler = function(input)
            return "read " .. tostring(input.path)
        end,
    }

    local tool_loop = require("tool_loop")

    function run(opts)
        opts.llm = { provider = "anthropic", model = "claude-haiku-4-5-20251001", api_key = "k" }
        return tool_loop.run(opts)
    end

    -- The recorded kinds in order, as one comparable string.
    function kinds_of(s)
        local out = {}
        for _, e in ipairs(s:events()) do
            table.insert(out, e.kind)
        end
        return table.concat(out, ",")
    end
"#;

/// A VM with the `knl` bridge registered, the Lua libraries `require`-able,
/// and the stubs of [`PRELUDE`] in place.
fn vm() -> Lua {
    let lua = Lua::new();
    agent_block_core::bridge::knl::register(&lua).expect("register knl");

    let mut registry = mlua_pkg::Registry::new();
    let mut memory = mlua_pkg::resolvers::MemoryResolver::new();
    for (name, source) in LIBS {
        memory = memory.add(*name, *source);
    }
    registry.add(memory);
    registry
        .install(&lua)
        .expect("install the require registry");

    lua.load(PRELUDE).exec().expect("prelude");
    lua
}

/// Load a chunk that is expected to pass.
fn exec(lua: &Lua, chunk: &str) {
    lua.load(chunk).exec().expect("chunk");
}

/// The run's facts land in the session in the order they happened, with the
/// fields the reserved kinds require.
#[test]
fn a_run_records_its_turns_in_order() {
    let lua = vm();
    exec(
        &lua,
        r#"
        local s = knl.session()
        queue_responses(tool_response("echo"), text_response("done"))
        local res = run({ prompt = "ask", tools = { echo_tool }, session = s })

        assert(res.ok == true, "run failed: " .. tostring(res.error))
        assert(res.content == "done", "content: " .. tostring(res.content))
        assert(res.turns == 2, "turns: " .. tostring(res.turns))

        assert(
            kinds_of(s) == "run_started,msg_user,model_response,tool_call,tool_result,model_response",
            "recorded: " .. kinds_of(s)
        )

        local evs = s:events()
        assert(evs[2].content == "ask", "prompt: " .. tostring(evs[2].content))
        assert(evs[3].turn == 1 and evs[3].content[1].type == "tool_use")
        assert(evs[3].usage.input_tokens == 10, "usage: " .. tostring(evs[3].usage.input_tokens))
        assert(evs[4].call_id == "call_1" and evs[4].name == "echo")
        assert(evs[4].args.path == "a.txt", "args: " .. tostring(evs[4].args.path))
        assert(evs[5].call_id == "call_1" and evs[5].ok == true)
        assert(evs[5].result == "read a.txt", "result: " .. tostring(evs[5].result))
        assert(evs[6].turn == 2 and evs[6].content[1].text == "done")
    "#,
    );
}

/// A session opened with a backend of its own is the one that takes the call:
/// the loop hands it a provider-neutral request and never reaches for the
/// transport its `llm` conf describes. The facts around the response are filed
/// under the turn the kernel stamped, not under the loop's own count.
#[test]
fn a_session_backend_takes_the_call_instead_of_the_wire() {
    let lua = vm();
    exec(
        &lua,
        r#"
        -- Queued and never asked for: the wire is what a session without a
        -- backend falls back to, and this one does not need it.
        queue_responses(text_response("from the wire"))

        local calls = 0
        local s = knl.session({
            backend = function(req)
                calls = calls + 1
                assert(type(req.messages) == "table", "the request must carry the conversation")
                assert(req.system == "sys", "system: " .. tostring(req.system))
                assert(req.tools[1].name == "echo", "the tool set must reach the backend")
                if calls == 1 then
                    return {
                        content = {
                            { type = "tool_use", id = "call_1", name = "echo",
                              input = { path = "a.txt" } },
                        },
                        usage = { input_tokens = 10, output_tokens = 5 },
                        stop_reason = "tool_use",
                    }
                end
                return {
                    content = { { type = "text", text = "from the session backend" } },
                    usage = { input_tokens = 3, output_tokens = 4 },
                    stop_reason = "end_turn",
                }
            end,
        })

        local res = run({ prompt = "ask", system = "sys", tools = { echo_tool }, session = s })

        assert(res.ok == true, "run failed: " .. tostring(res.error))
        assert(res.content == "from the session backend", "content: " .. tostring(res.content))
        assert(res.turns == 2, "turns: " .. tostring(res.turns))
        assert(calls == 2, "the bound backend ran " .. tostring(calls) .. " times")
        assert(queued_left() == 1, "the loop went to the wire despite the bound backend")
        assert(res.usage.input_tokens == 13, "input: " .. tostring(res.usage.input_tokens))
        assert(res.usage.output_tokens == 9, "output: " .. tostring(res.usage.output_tokens))

        assert(
            kinds_of(s) == "run_started,msg_user,model_response,tool_call,tool_result,model_response",
            "recorded: " .. kinds_of(s)
        )
        local evs = s:events()
        assert(evs[3].turn == 1, "the kernel stamped turn: " .. tostring(evs[3].turn))
        assert(evs[4].turn == 1 and evs[5].turn == 1, "the tool facts must echo the response's turn")
        assert(evs[5].result == "read a.txt", "result: " .. tostring(evs[5].result))
        assert(evs[6].turn == 2, "second response turn: " .. tostring(evs[6].turn))
    "#,
    );
}

/// A tool that fails is answered to the model *and* recorded, with `ok = false`
/// rather than as an absence.
#[test]
fn a_failed_tool_is_recorded_as_a_failure() {
    let lua = vm();
    exec(
        &lua,
        r#"
        local s = knl.session()
        queue_responses(tool_response("nonesuch"), text_response("gave up"))
        local res = run({ prompt = "ask", tools = { echo_tool }, session = s })

        assert(res.ok == true, "run failed: " .. tostring(res.error))
        local result = s:events()[5]
        assert(result.kind == "tool_result", "kind: " .. tostring(result.kind))
        assert(result.ok == false, "a failed tool was recorded as a success")
        assert(tostring(result.result):find("unknown tool", 1, true) ~= nil,
               "result: " .. tostring(result.result))
    "#,
    );
}

/// Passing a session changes nothing about what `run` returns.
#[test]
fn a_session_does_not_change_the_result() {
    let lua = vm();
    exec(
        &lua,
        r#"
        queue_responses(tool_response("echo"), text_response("done"))
        local without = run({ prompt = "ask", tools = { echo_tool } })

        queue_responses(tool_response("echo"), text_response("done"))
        local with = run({ prompt = "ask", tools = { echo_tool }, session = knl.session() })

        assert(without.ok == with.ok)
        assert(without.content == with.content)
        assert(without.turns == with.turns)
        assert(without.stop_reason == with.stop_reason)
        assert(#without.tool_calls == #with.tool_calls)
        assert(#without.messages == #with.messages)
        assert(without.usage.input_tokens == with.usage.input_tokens)
        assert(without.usage.output_tokens == with.usage.output_tokens)
        assert(without.usage.thinking_tokens == with.usage.thinking_tokens)

        local function key_list(res)
            local names = {}
            for k in pairs(res) do
                table.insert(names, k)
            end
            table.sort(names)
            return table.concat(names, ",")
        end
        assert(key_list(without) == "content,messages,ok,stop_reason,tool_calls,turns,usage",
               "keys without a session: " .. key_list(without))
        assert(key_list(with) == key_list(without), "the session added keys: " .. key_list(with))
    "#,
    );
}

/// The response is recorded *before* it joins the conversation: a session that
/// refuses the write leaves the loop's own messages without it.
#[test]
fn the_response_is_recorded_before_it_joins_the_conversation() {
    let lua = vm();
    exec(
        &lua,
        r#"
        local s = knl.session()
        queue_responses(text_response("never recorded"))

        -- Closed after the prompt was recorded and before the response comes
        -- back, so the model_response write is the one that gets refused.
        local res = run({
            prompt = "ask",
            session = s,
            on_request = function() s:close("closed mid-turn") end,
        })

        assert(res.ok == false, "a refused record must end the run")
        assert(tostring(res.error):find("session", 1, true) ~= nil,
               "the failure must name the session: " .. tostring(res.error))
        assert(res.turns == 0, "turns: " .. tostring(res.turns))
        assert(#res.messages == 1 and res.messages[1].role == "user",
               "the response joined the conversation before it was recorded")
        assert(kinds_of(s) == "run_started,msg_user,run_finished", "recorded: " .. kinds_of(s))
    "#,
    );
}

/// A session closed before the run is refused at the first record, before the
/// model is ever called.
#[test]
fn a_closed_session_fails_the_run_at_the_first_record() {
    let lua = vm();
    exec(
        &lua,
        r#"
        local s = knl.session()
        s:close("before the run")
        queue_responses(text_response("never asked for"))

        local res = run({ prompt = "ask", session = s })

        assert(res.ok == false, "a closed session must end the run")
        assert(tostring(res.error):find("session", 1, true) ~= nil,
               "the failure must name the session: " .. tostring(res.error))
        assert(res.turns == 0, "turns: " .. tostring(res.turns))
        assert(#res.messages == 0, "the prompt joined the conversation despite the refused record")
        assert(queued_left() == 1, "the model was called after the record was refused")
    "#,
    );
}

/// Spending the budget stops the run before the next turn, as a success: the
/// turn that spent it still completes, and the history says where it stopped.
#[test]
fn an_exhausted_budget_stops_before_the_next_turn() {
    let lua = vm();
    exec(
        &lua,
        r#"
        local s = knl.session({ budget = { tokens = 10 } })
        queue_responses(tool_response("echo"), text_response("unreached"))

        local res = run({ prompt = "ask", tools = { echo_tool }, session = s })

        assert(res.ok == true, "error: " .. tostring(res.error))
        assert(res.stop_reason == "budget_exhausted", "stop_reason: " .. tostring(res.stop_reason))
        assert(res.turns == 1, "turns: " .. tostring(res.turns))
        assert(s:remaining() == 0 and s:exhausted() == true)
        assert(queued_left() == 1, "a second model call was made after the budget ran out")
        assert(#res.tool_calls == 1, "the turn that spent the budget still ran its tools")
        assert(kinds_of(s) == "run_started,msg_user,model_response,tool_call,tool_result",
               "recorded: " .. kinds_of(s))
    "#,
    );
}

/// What the run reports as usage is what the session was charged.
#[test]
fn the_reported_usage_is_what_was_spent() {
    let lua = vm();
    exec(
        &lua,
        r#"
        local s = knl.session({ budget = { tokens = 1000 } })
        queue_responses(tool_response("echo"), text_response("done"))

        local res = run({ prompt = "ask", tools = { echo_tool }, session = s })
        assert(res.ok == true, "error: " .. tostring(res.error))

        -- 10 + 3 in, 5 + 4 out, 2 thinking.
        assert(res.usage.input_tokens == 13, "input: " .. tostring(res.usage.input_tokens))
        assert(res.usage.output_tokens == 9, "output: " .. tostring(res.usage.output_tokens))
        assert(res.usage.thinking_tokens == 2, "thinking: " .. tostring(res.usage.thinking_tokens))

        local spent = res.usage.input_tokens + res.usage.output_tokens + res.usage.thinking_tokens
        assert(s:remaining() == 1000 - spent, "remaining: " .. tostring(s:remaining()))

        -- The kernel's own fold over what it recorded agrees with the run.
        local u = s:view("usage")
        assert(u.input_tokens == res.usage.input_tokens)
        assert(u.output_tokens == res.usage.output_tokens)
        assert(u.thinking_tokens == res.usage.thinking_tokens)
        assert(u.model_calls == 2, "model_calls: " .. tostring(u.model_calls))
    "#,
    );
}

/// A response with no blocks is still recorded, as the one empty text block it
/// amounts to: an empty Lua table would reach the kernel as a mapping and be
/// rejected, and dropping the response would lose its usage with it.
#[test]
fn a_response_without_blocks_is_still_recorded() {
    let lua = vm();
    exec(
        &lua,
        r#"
        local s = knl.session()
        queue_responses({
            id = "msg_empty",
            role = "assistant",
            content = {},
            stop_reason = "end_turn",
            usage = { input_tokens = 1, output_tokens = 0 },
        })

        local res = run({ prompt = "ask", session = s })
        assert(res.ok == true, "error: " .. tostring(res.error))
        assert(res.content == "", "content: " .. tostring(res.content))

        local recorded = s:events()[3]
        assert(recorded.kind == "model_response", "kind: " .. tostring(recorded.kind))
        assert(#recorded.content == 1, "blocks: " .. tostring(#recorded.content))
        assert(recorded.content[1].type == "text" and recorded.content[1].text == "")
        assert(s:view("usage").input_tokens == 1, "the usage of the response was kept")
    "#,
    );
}
