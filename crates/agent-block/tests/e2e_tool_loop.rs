mod common;

use agent_block_testkit::server::MockLlm;
use agent_block_testkit::shapes::anthropic;
use predicates::prelude::*;
use serde_json::json;
use tempfile::tempdir;

/// `tool_loop` driven against a mock endpoint.
///
/// The mock answers by prompt, so one fixture can exercise every case: a tool
/// call followed by a plain answer, a call for a tool that was never handed in,
/// an adaptive set that changes between turns, and a model that never stops
/// asking (to prove `max_turns` bounds it).
///
/// The fixture prints `[TL] <label> = <value>`; the assertions below name the
/// labels whose value carries the property, rather than restating the fixture.
#[tokio::test]
async fn tool_loop_dispatches_only_the_tools_it_was_given() {
    let handle = MockLlm::anthropic(|req| {
        let prompt = req.body.to_string();

        if prompt.contains("loop forever") {
            // Never stops asking for tools.
            return anthropic::tool_use_response(vec![anthropic::tool_use(
                "t_forever",
                "alpha",
                json!({"v": "x"}),
            )]);
        }

        if prompt.contains("not there") {
            if req.has_tool_results {
                return anthropic::text_response("gave up");
            }
            return anthropic::tool_use_response(vec![anthropic::tool_use(
                "t_ghost",
                "ghost",
                json!({"v": "x"}),
            )]);
        }

        if prompt.contains("two turns") {
            if !req.has_tool_results {
                return anthropic::tool_use_response(vec![anthropic::tool_use(
                    "t_a",
                    "beta",
                    json!({"v": "1"}),
                )]);
            }
            // Second turn: `beta` is gone from the set, so asking for it now
            // must come back as unknown rather than being dispatched. The
            // third turn ends the run so the case does not ride to max_turns.
            if prompt.matches("unknown tool").count() == 0 {
                return anthropic::tool_use_response(vec![anthropic::tool_use(
                    "t_b",
                    "beta",
                    json!({"v": "2"}),
                )]);
            }
            return anthropic::text_response("stopped");
        }

        if req.has_tool_results {
            return anthropic::text_response("finished");
        }
        anthropic::tool_use_response(vec![anthropic::tool_use(
            "t_1",
            "alpha",
            json!({"v": "hello"}),
        )])
    })
    .spawn()
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url = handle.base_url.clone();
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        common::agent_block_cmd()
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("ANTHROPIC_BASE_URL_TEST", &url)
            .args(["-s", &common::fixture("tool_loop_basic.lua")])
            .assert()
            .success()
            .stdout(predicate::str::contains("[TL] done"))
            // dispatch reached the handler and the loop ended on a plain answer
            .stdout(predicate::str::contains("[TL] 1.ok=true"))
            .stdout(predicate::str::contains("[TL] 1.dispatched=alpha:hello"))
            .stdout(predicate::str::contains("[TL] 1.content=finished"))
            .stdout(predicate::str::contains("[TL] 1.usage_in>0=true"))
            // a tool outside the set is reported to the model, never invoked
            .stdout(predicate::str::contains("[TL] 2.unknown_as_result=true"))
            .stdout(predicate::str::contains("[TL] 2.not_dispatched=true"))
            // the set is re-resolved every turn
            .stdout(predicate::str::contains("[TL] 3.ok=true"))
            .stdout(predicate::str::contains("[TL] 3.tools_fn_turns=1,2,3"))
            .stdout(predicate::str::contains("[TL] 3.dispatched=beta:1"))
            // and the bound holds
            .stdout(predicate::str::contains("[TL] 4.ok=false"))
            .stdout(predicate::str::contains("[TL] 4.turns=2"))
            .stdout(predicate::str::contains("[TL] 4.error=true"))
            .stdout(predicate::str::contains("[TL] 5.on_turn=1:1,2:0"))
            .stdout(predicate::str::contains("[TL] 6.no_prompt=true"))
            .stdout(predicate::str::contains("[TL] 6.bad_provider=true"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    handle.ct.cancel();
}
