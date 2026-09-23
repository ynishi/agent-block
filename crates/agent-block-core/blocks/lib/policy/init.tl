--- policy — the values a caller's loop plugs into a knl device.
---
--- What this is
---   The kernel provides one BEAT and holds no loop. `knl.beat(session,
---   device)` runs one model call plus the tools that call asked for and
---   hands back an `Outcome`; composing beats is the caller's, on the spot.
---   A POLICY is a value that plugs into one of the device's seams — `fold`,
---   a `filter`, `cost`, `tool_policy`, `llm` — or that the caller's loop
---   consults between beats. It never runs a loop of its own, and the only
---   thing it does with the kernel's log is READ it: nothing here appends,
---   reserves, spends or closes.
---
---   Policies are opt-in, one at a time. A device built without them is the
---   plain kernel, behaving exactly as it did before this module existed —
---   which is what keeps the kernel free of the shell's habits rather than
---   growing them a beat at a time.
---
--- The eleven, and where each one plugs in
---
---     policy.window      -> a `fold`      the last n beats, or as many as
---                                         fit the model's window
---     policy.carry       -> a `filter`    one bounded note about the beat
---                                         that failed
---     policy.stagnation  -> a predicate   the loop asks it between beats:
---                                         is this run going in circles?
---     policy.retry       -> a predicate   the loop asks it about an Outcome:
---                                         is this failure worth asking again?
---     policy.escalate    -> `next`        the device for the next beat: this
---                                         one, or one with a stronger llm
---     policy.tokens      -> a `cost`      the beat's request in tokens, by
---                                         the Port's count, for a grant
---                                         tagged "tokens"
---     policy.result_cap  -> `tools`       one tool result may not outgrow a
---                                         share of the model's window
---     policy.repeat_cap  -> `tools`       the same call, again, with nothing
---                                         changed in between, is refused
---                                         past a count
---     policy.require_args-> `tools`       a call whose required arguments did
---                                         not all arrive is refused before
---                                         the handler runs
---     policy.thinking_cap-> a `filter`    where the reasoning has to stop for
---                                         the tool call after it to still fit
---     policy.verdict     -> a check       the loop runs it after every beat:
---                                         is the thing outside the
---                                         conversation true yet?
---
---   `window`, `carry`, `tokens`, `result_cap`, `repeat_cap`, `require_args`
---   and `thinking_cap` are device
---   fields (`knl.device{ fold = ..., filters = { ... }, cost = ..., tools = ... }`);
---   `stagnation`, `retry`, `escalate` and `verdict` are the loop's own and
---   the kernel never sees them. That
---   split is the whole shape of this module: a policy either changes what
---   one beat SENDS (or reserves), or it decides what the loop does BETWEEN
---   beats. Nothing here decides what a beat does while it runs — that is the
---   kernel's, and it is not a seam.
---
--- Where each one lives
---   `policy` is one module in five files, and this one is the door: it
---   gathers the eleven, publishes `policy.shapes`, and runs the registry
---   gate. The policies are grouped by what they read —
---
---     policy/room.lua     window, tokens, result_cap, thinking_cap — the
---                         four that size against the model's window, and
---                         the one split of it they all read (`request_limit`
---                         for the prompt, `reply_room` for the reply), off
---                         the Port's profile; `policy.split` hands that
---                         split back as a value, for a caller that wants to
---                         see the numbers a run is sized by — a reading,
---                         not a twelfth policy
---     policy/tools.lua    repeat_cap, require_args — the two that wrap the
---                         device's tools map
---     policy/carry.lua    carry — the one filter that reads the log
---     policy/loop.lua     verdict, stagnation, retry, escalate — what the
---                         loop asks between beats
---     policy/shared.lua   the defaults, the log readers, the contract
---                         prelude and the shapes more than one file hands
---                         out; not a policy, not exported
---
---   A caller requires `policy` and nothing under it; the files are
---   `policy.<name>` for `vendor`, which copies the module whole. Every rule
---   in this header holds in every file, and a file's own header says only
---   what is particular to it.
---
--- One shell, two packs — and where a model's limits go
---   The shell above the kernel is this module and `supervisor`, and the two
---   are a SET: a loop is composed from both — policies on the device and
---   between beats, the supervisor for the session tree — and neither is a
---   loop by itself. A third pack beside them is not how the shell grows.
---
---   In particular, a model's limits are not a pack. A narrow context window,
---   a model that re-reads what it has already seen, one that cannot copy an
---   `expect` exactly, one that stalls on a hard error, a task too large for
---   one loop — each of these is answered where the kernel already left a
---   seam for it, and by what the Port already knows:
---
---     the window            the Port declares it (`LLMPort:profile`), and
---                           `window{ fit }` / `tokens` ask it; `fit.reserve`
---                           is the reply's room, for a wire that sends no
---                           cap of its own
---     what a tool may do    `tool_policy` on the device, reading the log
---     what a request says   `fold` (what is sent) and `filters` (a note)
---     when to stop          a predicate the loop asks (`stagnation`, and
---                           its siblings a loop may write)
---     too big for one loop  `supervisor.child` / `parallel` / `merge`
---
---   The thresholds a particular model wants (how many bytes a read may
---   return, how many times a range may be re-read) are opts on those
---   factories, and the model's name never appears in this module. Before a
---   new module is written for a model, the question is whether the kernel
---   lacks a seam (then the kernel is the change) or whether the Port lacks a
---   declaration (then the adapter is) — and one of the two has so far been
---   the answer every time.
---
--- Opts are policy, the session is an argument
---   `knl.device` and `knl.open` split policy from state: a device holds
---   `llm` / `tools` / `fold` / `filters` and refuses `owner` / `store` /
---   `session`, because a device is a frozen value that several sessions can
---   share while a session is durable state one kernel owns. This module
---   keeps the same line, and it is the answer to the one question every
---   log-reading policy raises — how does it reach the log?
---
---     * a factory's opts are POLICY. A session in them would be a typo, and
---       is refused as one;
---     * the session arrives as an ARGUMENT, in whatever signature the
---       returned value already has.
---
---   For `window` that argument is `events`: a fold is handed the log by
---   beat, so it never needs a handle. For `stagnation` it is `session`: the
---   predicate the loop calls takes one. `carry` is the only one whose
---   returned signature has no room for it — a filter is `fn(request) ->
---   request` and the kernel will pass nothing else — so the factory answers
---   a BINDER instead:
---
---       local device = knl.device({
---           llm = llm,
---           fold = policy.window({ tail = 4 }),
---           filters = { policy.carry({ max_bytes = 512 })(session) },
---       })
---
---   `policy.carry{...}` is a session-free value a caller can hold and bind
---   to whichever session it is driving, exactly as one device is shared
---   across sessions. The alternative — `policy.carry{ session = s }` — would
---   put state in a policy constructor's config, which is the one thing the
---   kernel's own constructors are written to refuse.
---
--- No policy holds state
---   Nothing in this module remembers anything between beats: not in the
---   module, not in a factory's closure. A factory closure holds only what it
---   was configured with (`tail`, `max_bytes`, the thresholds, the strong
---   llm), and those are frozen at construction the way a device's fields
---   are. Everything else is derived from the log on every call, so two
---   processes reading the same session reach the same verdict and a resumed
---   session does not start counting from zero.
---
---   Which of them read the log, and which read nothing:
---
---     window      reads the log — the `events` beat handed it
---     carry       reads the log — `session:events()`, through the binder
---     stagnation  reads the log — `session:events()`, per call
---     retry       reads NO log: the `Outcome` it is given, plus `attempt`,
---                 which is the caller's own count and is passed in
---     escalate    reads NO log: the `Outcome` it is given
---     repeat_cap  reads the log — `session:events()`, through the binder,
---                 on every call it is asked to answer
---     require_args reads NO log: the arguments it is handed, against the
---                 `required` list the tool already declares
---     thinking_cap reads NO log: the request it is handed, counted by the
---                 Port, against the room the Port's profile leaves the reply
---     verdict     reads the log — `session:events()`, per call, and only
---                 when `timeout` is a table: what the checks took are gaps
---                 between the kernel's stamps, read by the `measure` the
---                 caller chose, not a number the factory kept. A number
---                 for `timeout` reads nothing
---
---   The escape hatch the design allows — an explicit `run` table the CALLER
---   creates and owns for one shell run — is not used by any of the five,
---   because nothing any of them needs is missing from the log or from an
---   argument. If a later policy does need one, it takes that table as an
---   argument like any other; a module-level global would be the same state
---   with nobody owning it.
---
--- Reading the log: `session:events()`, not a query view
---   `carry` and `stagnation` both read `session:events()` rather than
---   `knl.views.tool_pairs` / `knl.views.beats`, and it is a choice rather
---   than an oversight. Neither question is answerable from those views:
---   `llm_call_failed` is not a tool pair and has no row in `tool_pairs`, and
---   a beat that made no tool call at all — the very thing `no_progress` is
---   about — is exactly the beat that leaves no row behind. A view would
---   answer half of each question and the log would still have to be read for
---   the other half, which is two reads and two ways to be wrong.
---
---   A read that fails is not caught here. A closed session or a store that
---   will not answer raises out of `session:events()`, and the raise is
---   reported where the policy was called from: for `carry` that is beat,
---   which turns a raising filter into `Outcome.err("filter")`. Swallowing it
---   would hide a dead store behind a policy that quietly does nothing.
---
---   A read that was CUT SHORT is refused the same way (`whole_log`). The
---   kernel's read is bounded and answers `rows, truncated`, and the cap counts
---   forward — so a truncated read is the FRONT of the log and both of these
---   policies are asking about its end. Folding one would let `carry` build a
---   note from a beat that is not the last, and `stagnation` judge repetition
---   over beats the run has already left behind: wrong answers that look right.
---
---   A BOUNDED TAIL READ WOULD NOT FIX IT, which is why neither policy takes
---   one. `session:view("tail", n)` bounds EVENTS and both of these reason in
---   BEATS, and a beat writes as many events as the model asked for tool calls:
---   no `n` is a beat count, and a window can begin in the middle of a beat.
---   `beats_of` cannot tell that from a whole one, so `failure_note` would
---   report "a tool call failed" for a pair whose `tool_call` half was outside
---   the window, and `made_progress` would call a beat idle whose only
---   `tool_call` was sliced off — the same hole, moved somewhere quieter. The
---   query views are ruled out above for reasons of their own. A run long
---   enough to hit the cap wants a window on the request (`policy.window`) or
---   a fresh session, and that is a caller's decision, so the policy says so
---   and stops.
---
--- Beats, as this module sees them
---   The kernel stamps a `beat` id on every event one beat writes and does
---   not number them (`knl`'s header). So a beat, here, is derived: the
---   events carrying one id, in the order they were written, as
---   `{ id = <string>, events = { ... } }` — `policy.shapes.beat_record`.
---   That record is what a custom `signature` is handed, and it is the only
---   place this module names a structure of its own.
---
---   Events with no `beat` — the caller's seed, `session_*`, `budget_*` — are
---   part of no beat and are not in any record. They are still part of the
---   LOG, which is why `window` slices the event list rather than the beat
---   list.
---
--- The shapes are declared and the registry is executed
---   Every public interface here — each factory's opts, and the arguments and
---   return of every function a factory hands back — is an lshape published
---   through `policy.shapes`, and `policy.shapes.api` names the shape of
---   every argument of every export. In dev mode (LSHAPE_CHECK) each declared
---   export is wrapped once, at load, by a gate that holds the call to its
---   entry; prod installs no wrapper and pays nothing. This is the same
---   arrangement `knl.shapes` has and for the same reason: a registry nobody
---   runs is prose with a table around it.
---
---   Because the gate is dev-only, every check a call must not get through
---   WITHOUT is written beside it as an explicit check and is loud in both
---   modes — an unknown option, a threshold that is not a whole number, an
---   `llm` that cannot be called. A policy built out of a mistyped config
---   must fail at the line that built it, not at the beat that used it.
---
---   And a check that is loud in both modes must not be ANSWERED by the gate,
---   which is the subtler half of the same rule. The gate wraps the export, so
---   whatever it judges it judges first; if it were handed the closed opts
---   shape it would be the thing that reports an unknown option, in dev only,
---   in different words. So the two judgements are split and each has one
---   owner: `only` says whether a key is declared (both modes, and it is the
---   message a caller reads), the registry says whether the declared keys have
---   the right shape (dev only). `opts_contract` is where that split is made.
---
--- Deliberately not here
---   Summarising a window instead of dropping it, a cost policy, a
---   `tool_policy` gate, parallel or speculative beats, and any policy that
---   would need to write to the log to work. Each is deferred until a real
---   loop asks for it.
---
--- Written in Teal, all six files, and embedded through `include_tl!`
--- (`host.rs`); the Lua the host holds is what Teal generates, without
--- these comments. The kernel is read through its own record (`knl` is
--- Teal too); the records the files share are `policy.shared`'s.
local lshape = require("lshape")
local shape = lshape.check
local shared = require("policy.shared")
local room = require("policy.room")
local tools = require("policy.tools")
local carry = require("policy.carry")
local loop = require("policy.loop")

--- The public contracts, as data (`policy.shapes`), and the registry
--- (`policy.shapes.api`).
local record Shapes
    window_opts: lshape.Schema
    split_opts: lshape.Schema
    split: lshape.Schema
    carry_opts: lshape.Schema
    require_args_opts: lshape.Schema
    thinking_cap_opts: lshape.Schema
    stagnation_opts: lshape.Schema
    retry_opts: lshape.Schema
    escalate_opts: lshape.Schema
    beat_record: lshape.Schema
    tool_pair: lshape.Schema
    stop_reason: lshape.Schema
    api: {string:shared.ApiEntry}
end

local record M
    window: room.WindowFactory
    tokens: room.TokensFactory
    result_cap: room.ResultCapFactory
    thinking_cap: room.ThinkingCapFactory
    split: room.SplitFn
    repeat_cap: tools.RepeatCapFactory
    require_args: tools.RequireArgsFactory
    carry: carry.CarryFactory
    verdict: loop.VerdictFactory
    stagnation: loop.StagnationFactory
    retry: loop.RetryFactory
    escalate: loop.EscalateFactory
    shapes: Shapes
end

-- ============================================================
-- The eleven, gathered
-- ============================================================
--
-- Each file exports the policies it owns; this table is the one a caller
-- requires. Listed by name rather than copied by `pairs` so the public
-- surface of `policy` is written down here, once, and a function a file
-- happens to export beside its policies does not become part of it.

M.window = room.window
M.tokens = room.tokens
M.result_cap = room.result_cap
M.thinking_cap = room.thinking_cap
-- The window's split as a value — a reading beside the four, not a policy.
M.split = room.split
M.repeat_cap = tools.repeat_cap
M.require_args = tools.require_args
M.carry = carry.carry
M.verdict = loop.verdict
M.stagnation = loop.stagnation
M.retry = loop.retry
M.escalate = loop.escalate

-- ============================================================
-- shapes — the public contracts of this module
-- ============================================================
--
-- Every factory's opts is declared in the file that owns the policy and
-- gathered here, with the three shapes more than one policy hands out. The
-- set of keys is the module's contract: a file's `shapes` joins it by being
-- listed, not by being found.

M.shapes = {
    window_opts = room.shapes.window_opts,
    split_opts = room.shapes.split_opts,
    split = room.shapes.split,
    carry_opts = carry.shapes.carry_opts,
    require_args_opts = tools.shapes.require_args_opts,
    thinking_cap_opts = room.shapes.thinking_cap_opts,
    stagnation_opts = loop.shapes.stagnation_opts,
    retry_opts = loop.shapes.retry_opts,
    escalate_opts = loop.shapes.escalate_opts,
    beat_record = shared.BEAT_RECORD,
    tool_pair = shared.TOOL_PAIR,
    stop_reason = shared.STOP_REASON,
}

-- ============================================================
-- The API registry
-- ============================================================
--
-- One entry per public export, naming the shape of what goes in and what
-- comes out — the same form `knl.shapes.api` uses, so the two read alike and
-- one spec can be written against the other's shape.
--
-- `args` is an ordered list of `{ shape, desc }`, one per positional
-- argument, and it is what the dev-mode gate below RUNS. `members` names the
-- functions a factory hands BACK: they are values this module produces rather
-- than exports it owns, so — exactly like `device:with` in knl — their
-- entries are declared and walked by the spec but nothing wraps them.

-- The `*_ARG` shapes each file declares are the OPEN twins of the
-- published contracts (see `shared.opts_contract`). The registry holds a
-- call to the SHAPE of the options it declared; whether a key is declared
-- at all is `only`'s judgement, made in both modes, and the gate must not
-- answer it first with a message of its own.

M.shapes.api = {}
local registries: {{string:shared.ApiEntry}} = { room.api, tools.api, carry.api, loop.api }
for _, api in ipairs(registries) do
    for name, entry in pairs(api) do
        if M.shapes.api[name] ~= nil then
            error("policy: two files declare `" .. name .. "` in the api registry", 0)
        end
        M.shapes.api[name] = entry
    end
end
M.shapes.api.shapes = {
    args = {},
    returns = "this registry: every shape above, plus `api`",
}

-- ============================================================
-- The registry, executed
-- ============================================================
--
-- In dev mode each declared export is replaced, once, here at load, by a
-- wrapper that holds the call to its entry. Prod installs nothing and a call
-- pays nothing — which is why every check a factory must not be built without
-- is written beside it, loud in both modes.
--
-- What the gate judges is the shape of the arguments that were PASSED. An
-- argument nobody supplied is left to the function: `policy.window()` must go
-- on raising its own "tail must be a whole number" rather than a shape
-- violation about an opts table that was never there.
--
-- And what it judges about an opts table is the shape of the keys that are
-- DECLARED, never whether an undeclared one is present — that judgement is
-- `only`'s, in both modes (`opts_contract`). A gate that answered it would
-- make the module say two different things about one typo depending on an
-- environment variable, which is a divergence between test harnesses waiting
-- to happen (and it was one: the spec runner sets LSHAPE_CHECK=1, and three
-- refusal cases that passed under a bare runner failed under it).

local function arg_checked(name: string, fn: function(...: any): any..., declared: {shared.ApiArg}): function(...: any): any...
    return function(...: any): any...
        for i = 1, #declared do
            local value = select(i, ...)
            if value ~= nil then
                local declared_arg = declared[i]
                shape.assert_dev(
                    value,
                    declared_arg.shape,
                    name .. " arg " .. tostring(i) .. " (" .. declared_arg.desc .. ")"
                )
            end
        end
        return fn(...)
    end
end

if shape.is_dev_mode() then
    local exports = M as {string:any}
    for name, entry in pairs(M.shapes.api) do
        local export = exports[name]
        if type(export) == "function" then
            exports[name] = arg_checked("policy." .. name, export as function(...: any): any..., entry.args)
        end
    end
end

return M
