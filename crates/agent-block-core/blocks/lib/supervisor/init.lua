--- supervisor — running the session structure the kernel only records.
---
--- What this is
---   The kernel records the FACTS of a session tree and refuses nothing about
---   its shape. A child's opening names its parent, the child's quota is moved
---   out of the parent's ledger in one transaction, and a close records the
---   children that had not ended (`session_closed.data.open_children`). That
---   is the whole of it: `knl.views.tree` reads those two fields back, and no
---   rule inside the kernel says when a child may open, how many may run at
---   once, or what happens to one whose parent is failing.
---
---   This module is the shell layer that RUNS that structure. It opens a child
---   inside a bracket so a child always closes before its parent, it runs
---   siblings concurrently on `std.task`, and it reads several sessions into
---   one request. It holds no state of its own and no loop; every loop is the
---   caller's, exactly as with `knl.beat`.
---
--- The three, and what each one is for
---
---     supervisor.child      one child, opened and closed around a body
---     supervisor.parallel   siblings, run at once in one nursery
---     supervisor.merge      a `fold` that reads several sessions into one
---                           request
---
---   `child` is the primitive and the other two are written in terms of what
---   it promises: `parallel` runs it once per sibling inside a `std.task`
---   scope, and `merge` is a device seam (a `fold`) rather than something that
---   runs anything at all.
---
--- The bracket is the kernel's, not a second one
---   `child` opens through `knl.session({ parent = parent, budget = {
---   from_parent = n } }, fn)`. The kernel's own bracket already passes
---   `parent` through to `knl.open`, so the discipline a caller gets is the
---   kernel's single implementation of it: the body runs under `pcall`, the
---   session is closed either way, a body error wins over a close that failed
---   on its way out, and the loser is logged as a record rather than dropped.
---   Writing that again here would be a second bracket to keep in step.
---
---   What this module adds around it is the ONE thing the bracket cannot say:
---   whether the failure that came out of it was the allocation being refused
---   — in which case no child was opened and there is nothing to close — or
---   the body raising inside a child that did open. The two are told apart by
---   whether the body was entered, which is a fact this module has and the
---   bracket does not.
---
--- A refusal is an answer, a raise is a raise
---   `child` answers `nil, err` when the parent's balance would not cover the
---   allocation (`err.kind == "refused"`, the kernel's reading of the raise)
---   and re-raises anything the body threw. That asymmetry is the kernel's:
---   a short balance is a DECISION recorded in the log and nothing is wrong,
---   while a body that raised is a failure the caller wrote and must see.
---
---   Nothing comes back to the parent when a child closes. An allocation is a
---   spend from the parent's side — the units left the balance and are not
---   coming back — so a supervisor that "returned the unused part" would be
---   inventing a move the kernel does not have.
---
--- No state, and no run scope
---   Nothing here remembers anything between calls: not in the module, not in
---   a closure. `merge` freezes the sessions it was told to read and the
---   decoder it needs, the way a device freezes its fields, and everything
---   else is derived from the log or from an argument on every call.
---
---   The run scope the design allows — a table a bracket creates for a policy
---   that needs memory ACROSS beats — is not here, because nothing needs one:
---   every policy reads the log. `child`'s bracket is where such a table would
---   be created if a policy ever did, and until then it would be state with
---   nobody owning it.
---
--- Vocabulary
---   A session is a session, never a "task": `std.task` runs tasks, and a
---   session is the durable log one of them drives. A child's allocation is an
---   `amount` of the parent's `budget`, a beat is a beat, and what a beat
---   answers is an `Outcome`. Nothing here is a "turn" and nothing is
---   "charged".
---
--- The shapes are declared and the registry is executed
---   Every public interface is an lshape published through
---   `supervisor.shapes`, and `supervisor.shapes.api` names the shape of every
---   argument of every export. In dev mode (LSHAPE_CHECK) each declared export
---   is wrapped once, at load, by a gate that holds the call to its entry;
---   prod installs no wrapper and pays nothing. This is `policy`'s arrangement
---   and `knl`'s, for the same reason: a registry nobody runs is prose with a
---   table around it.
---
---   And, like `policy`, the two judgements about an opts table have different
---   owners. `only` says whether a key is declared at all and is loud in BOTH
---   modes; the registry says whether the declared keys have the right shape
---   and is welcome to be dev-only. A gate that answered the first would make
---   this module say two different things about one typo depending on an
---   environment variable.
---
--- Deliberately not here
---   A run scope, a supervision strategy (restart / backoff / one-for-all), a
---   policy over a subtree, waiting on a child from outside its bracket, and
---   any read of the tree that decides something. Each is deferred until a
---   real loop asks for it.

local kernel = require("knl")
local lshape = require("lshape")
local T = lshape.t
local shape = lshape.check

local M = {}

-- ============================================================
-- Shared helpers
-- ============================================================

--- The JSON-array tag the bridge's converter honours (`lua_to_json` reads
--- `__jsontype = "array"`), the same one `knl.fold` puts on every array it
--- builds. `merge` rebuilds the messages array, so it re-tags: an array that
--- lost the tag on the way out of a fold would cross the boundary as `{}`.
local ARRAY_TAG = { __jsontype = "array" }

local function tag_array(t)
    return setmetatable(t, ARRAY_TAG)
end

--- Whether `v` is a whole number of at least `min`.
---
--- The bound is checked here rather than left to the shape because a dev-only
--- gate would let `amount = 0` through in prod — an allocation of nothing,
--- which is a child that can never beat and a supervisor that looks like it
--- worked.
local function whole_at_least(v, min)
    return type(v) == "number" and v % 1 == 0 and v >= min
end

--- Reject an option this call does not know. Loud, and in prod too.
---
--- `who` names the call and, for a nested table, the field: the message a
--- caller reads is the one place a typo is explained, so it says where.
local function only(opts, allowed, who)
    for k in pairs(opts) do
        if not allowed[k] then
            error(who .. ": unknown option '" .. tostring(k) .. "'", 3)
        end
    end
end

--- Whether `v` can be reached as a knl session: the kernel's userdata, or the
--- faithful Lua stand-in a spec drives. No schema can ask a userdata what it
--- can do, so this is the type test and the kernel makes the rest of the
--- judgement when the handle is used.
local function is_handle(v)
    return type(v) == "table" or type(v) == "userdata"
end

--- Read a raised kernel failure back as data — `{ kind?, method?, retryable,
--- message }`, `knl.shapes.error`.
---
--- The reader is the BRIDGE's (`knl.error`), not the Lua module's: a Rust
--- callback cannot raise a table, so it raises the attributed text `knl:
--- <method>: <kind>: <message>` and publishes the reading beside it. The
--- lookup is lazy for the same reason knl's own is — a spec that installs a
--- bridge after this module loaded still reaches it.
---
--- A VM with no bridge gets the same fields with nothing read out of them, so
--- a caller's `err.message` is always there and `err.kind` is the field it has
--- to ask for.
local function read_error(e)
    local bridge = rawget(_G, "knl")
    if type(bridge) == "table" and type(bridge.error) == "function" then
        local ok, structured = pcall(bridge.error, e)
        if ok and type(structured) == "table" then
            return structured
        end
    end
    return { retryable = false, message = tostring(e) }
end

--- `std.task`, or a loud error naming what is missing.
---
--- The nursery is the host's, and a VM without it (the pure spec runner) is
--- not a VM `parallel` can run in at all. Saying so is better than indexing
--- nil three frames deeper.
local function nursery()
    local std = rawget(_G, "std")
    local task = type(std) == "table" and std.task or nil
    if type(task) ~= "table" or type(task.scope) ~= "function" then
        error("supervisor.parallel: std.task is not available in this VM (it is the host's nursery)", 3)
    end
    return task
end

--- `std.json.decode`, or a loud error naming what is missing.
---
--- `merge` needs it because the log's `data` column crosses the query boundary
--- as TEXT: a query answers the columns SQLite holds, and the structured half
--- of an event is JSON in one of them. `session:events()` decodes on the way
--- out and a query does not, which is the difference between reading your own
--- stream and reading somebody else's.
local function json_decoder()
    local std = rawget(_G, "std")
    local json = type(std) == "table" and std.json or nil
    if type(json) ~= "table" or type(json.decode) ~= "function" then
        error("supervisor.merge: std.json.decode is not available in this VM (the `data` column is text)", 3)
    end
    return json.decode
end

-- ============================================================
-- shapes — the public contracts of this module
-- ============================================================
--
-- The shapes that describe a knl value are the kernel's own — `event_base`,
-- `request` — reached through `kernel.shapes` rather than retyped. A second
-- copy of a contract is a contract with two versions.

--- A Lua function, as a shape. `lshape.t` exposes only the five prims it
--- names, so these two are built from the same plain-data schema form.
local FUNCTION = setmetatable({ kind = "prim", prim = "function" }, lshape.t._internal.schema_mt)
local USERDATA = setmetatable({ kind = "prim", prim = "userdata" }, lshape.t._internal.schema_mt)

--- A session handle: the kernel's userdata, or a spec's stand-in.
local SESSION_HANDLE = T.any_of({ T.table, USERDATA })

--- An opts contract, as the two shapes it has to be: CLOSED is the published
--- one and the one a call asserts in dev, OPEN is the same fields with the
--- "no other key" judgement removed, for the registry gate alone.
---
--- The split has one purpose — keeping `only` the single owner of "is this key
--- declared", in both modes. `policy` learned it the hard way: handed the
--- closed shape, the dev-only gate became the thing that reported an unknown
--- option, in different words, so the module said two different things about
--- one typo depending on `LSHAPE_CHECK`.
local function opts_contract(fields)
    return T.shape(fields, { open = false }), T.shape(fields)
end

--- What a parent moves to a child: an `amount` of its own balance, counted in
--- `tag` (the parent's unit when it is left out).
---
--- `amount` here is the kernel's `budget.from_parent` — this module says
--- `amount` because that is the word a caller granting a budget already uses,
--- and translates at the syscall. There is no `desc`: a grant is an owner
--- allowing and carries a reason, an allocation is a move and what the log
--- records about why is the parent it names.
local CHILD_BUDGET, CHILD_BUDGET_ARG = opts_contract({
    amount = T.integer:min(1):describe("units, moved out of the parent's balance"),
    tag = T.string:is_optional(),
})

--- What `supervisor.child` is configured with. The budget is the whole of it:
--- `owner` is the parent's and the kernel inherits it, `store` is the parent's
--- database by construction, and `parent` is the argument.
---
--- The two twins are built by hand rather than through `opts_contract` because
--- the nested table has twins of its own and they have to line up: the OPEN
--- opts carry the OPEN budget, or the registry would answer an unknown
--- `budget` key in dev with a message of its own and `only` would lose the
--- judgement one level down.
local CHILD_OPTS = T.shape({ budget = CHILD_BUDGET }, { open = false })
local CHILD_OPTS_ARG = T.shape({ budget = CHILD_BUDGET_ARG })

--- One entry of `parallel`'s list: the opts a child is opened with, and the
--- body to run in it.
---
--- The opts carry the OPEN twin, because `child` refuses an undeclared key
--- itself, in both modes; the registry judging it here would answer first and
--- only in dev.
local CHILD_ENTRY = T.shape({
    opts = CHILD_OPTS_ARG,
    fn = FUNCTION,
}, { open = false })

--- What a sibling's slot in `parallel`'s answer holds.
---
--- Two forms and no third: a child either returned or it did not, and a slot
--- is never nil — an index that answered nothing would make the result array
--- unreadable by position, which is the one thing it is for.
local RESULT_SLOT = T.any_of({
    T.shape({
        ok = T.literal(true),
        values = T.table:describe("what the body returned, packed (`n` is the count)"),
    }, { open = false }),
    T.shape({
        ok = T.literal(false),
        err = T.any:describe("the error table a refusal read back, or whatever the body raised"),
        cancelled = T.boolean:describe("this sibling unwound on the scope's cancel"):is_optional(),
    }, { open = false }),
})

--- What `parallel` answers: one slot per entry, in the order they were given.
local RESULTS = T.array_of(RESULT_SLOT)

--- How a sibling's failure is joined to the rest.
---
---   "isolate"          the default: a failure is that sibling's, and the
---                      others run to completion
---   "cancel_on_error"  the first failure cancels the scope, so cooperative
---                      siblings unwind
local JOINER = T.one_of({ "isolate", "cancel_on_error" })

--- What `supervisor.parallel` is configured with.
---
--- One table rather than a bare joiner string, so the deadline has somewhere
--- to go: `timeout_ms` and `joiner` are two knobs on one run and a second
--- positional argument would have to be told apart by type.
local PARALLEL_OPTS, PARALLEL_OPTS_ARG = opts_contract({
    joiner = JOINER:is_optional(),
    timeout_ms = T.integer:min(1):describe("wall time for the whole scope, teardown included"):is_optional(),
})

--- A session `merge` reads: its id, or a handle to ask for one.
local MERGE_SESSION = T.any_of({ T.string, T.table, USERDATA })

--- What `supervisor.merge` is configured with: the two knobs any other read
--- has. There is no `sessions` key — the set is the argument, because it is
--- what the fold is ABOUT rather than how it is tuned.
local MERGE_OPTS, MERGE_OPTS_ARG = opts_contract({
    limit = T.integer:min(1):describe("rows before the read is cut off"):is_optional(),
    timeout_ms = T.integer:min(1):describe("how long the read may take"):is_optional(),
})

M.shapes = {
    child_opts = CHILD_OPTS,
    child_budget = CHILD_BUDGET,
    child_entry = CHILD_ENTRY,
    parallel_opts = PARALLEL_OPTS,
    merge_opts = MERGE_OPTS,
    joiner = JOINER,
    result_slot = RESULT_SLOT,
    results = RESULTS,
}

-- ============================================================
-- child — one child, opened and closed around a body
-- ============================================================

--- Hold a child's opts to their contract. Loud in both modes, because a
--- dev-only check would let a mistyped allocation through in prod as a child
--- that opened under something nobody asked for.
---
--- `who` names the call site — `supervisor.child` or the entry of a
--- `parallel` list — so a bad entry in a list of five says which one.
local function child_contract(opts, who)
    if type(opts) ~= "table" then
        error(who .. ": opts must be a table { budget = { amount, tag? } }", 3)
    end
    only(opts, { budget = true }, who)
    local budget = opts.budget
    if type(budget) ~= "table" then
        error(who .. ": budget is required — a child opens out of the parent's balance", 3)
    end
    only(budget, { amount = true, tag = true }, who .. " budget")
    if not whole_at_least(budget.amount, 1) then
        error(who .. ": budget.amount must be a whole number >= 1, got " .. tostring(budget.amount), 3)
    end
    if budget.tag ~= nil and type(budget.tag) ~= "string" then
        error(who .. ": budget.tag must be a string", 3)
    end
end

--- Open a child around `fn` and report which of the three things happened.
---
---   "ran"      the body returned; the payload is the `pcall` pack and the
---              values start at index 2
---   "refused"  the allocation did not happen; the payload is the kernel's
---              reading of the raise, and NO child was opened
---   "raised"   the body raised inside a child that did open; the child was
---              closed with reason "error" by the bracket and the payload is
---              the raised value, on its way back up
---
--- The three are told apart by `entered`, which is set as the body begins.
--- The bracket cannot make that distinction — from inside `knl.session` an
--- open that raised and a body that raised are both "the call failed" — and it
--- is exactly the distinction `child`'s contract rests on.
local function attempt(parent, opts, fn)
    local entered = false
    local returned = table.pack(pcall(kernel.session, {
        parent = parent,
        budget = { from_parent = opts.budget.amount, tag = opts.budget.tag },
    }, function(child)
        entered = true
        return fn(child)
    end))
    if returned[1] then
        return "ran", returned
    end
    if entered then
        return "raised", returned[2]
    end
    return "refused", read_error(returned[2])
end

--- Open a child of `parent` and run `fn` in it.
---
---     local sum, err = supervisor.child(parent, { budget = { amount = 4 } },
---         function(child)
---             child:append({ kind = "msg_user", data = { content = task } })
---             return kernel.beat(child, device)
---         end)
---
--- The child lands on the parent's database, its opening names the parent, and
--- its quota is moved out of the parent's balance — one write, both ledgers.
--- `owner` is not an option: the kernel gives the child the parent's, which is
--- what makes a subtree one owner's.
---
--- The lifecycle is the kernel's bracket, so a child always closes before its
--- parent does and closes even when the body raises. Nothing is returned to
--- the parent's ledger on the way out — an allocation is a spend.
---
--- Three answers, and they are different in kind:
---
---   * the body returned      — its values are handed straight back
---   * the allocation refused — `nil, err`, with `err.kind == "refused"`. No
---                              child was opened, nothing was closed, and the
---                              refusal is on the parent's ledger
---   * the body raised        — the child is closed with reason "error" and
---                              the raise goes on up, unchanged
---
--- A caller that wants the refusal as a raise asks for it (`assert(child(...))`);
--- a caller that wants the raise as a value pcalls. Answering both the same way
--- would be this module deciding which of them is the exception.
---
--- @param parent userdata|table  the session the child is opened from
--- @param opts table  { budget = { amount = <whole number >= 1>, tag? } }
--- @param fn function  fn(child) -> ...
--- @return ...  whatever `fn` returned, or `nil, err` on a refusal
function M.child(parent, opts, fn)
    if not is_handle(parent) then
        error("supervisor.child: the first argument must be a knl session (from knl.open / knl.session)", 2)
    end
    if type(fn) ~= "function" then
        error("supervisor.child: the third argument must be a function fn(child)", 2)
    end
    child_contract(opts, "supervisor.child")
    shape.assert_dev(opts, CHILD_OPTS, "supervisor.child opts")

    local status, payload = attempt(parent, opts, fn)
    if status == "ran" then
        return table.unpack(payload, 2, payload.n)
    end
    if status == "refused" then
        return nil, payload
    end
    -- Level 0: the body's error travels as it was raised. Adding this line's
    -- position would make the supervisor look like where it went wrong.
    error(payload, 0)
end

-- ============================================================
-- parallel — siblings, run at once in one nursery
-- ============================================================

--- Whether a raise is the scope's cancel reaching a cooperative child.
---
--- `std.task` raises the text "task cancelled" at every checkpoint once the
--- effective token fires, and that is the only handle on it from Lua — the
--- raise carries no class. So the reading is by text, in one place, and what
--- it produces is a `cancelled` flag beside the error rather than a different
--- kind of slot: the sibling still failed, and what it failed of is worth
--- saying.
local function is_cancellation(raised)
    return tostring(raised):find("task cancelled", 1, true) ~= nil
end

--- Run every entry of `children` at once, one child session each.
---
---     local results = supervisor.parallel(parent, {
---         { opts = { budget = { amount = 4 } }, fn = search },
---         { opts = { budget = { amount = 4 } }, fn = summarise },
---     })
---
--- Each entry goes through `child`, so every promise above holds per sibling:
--- the allocation is one write on the parent's ledger, the bracket closes the
--- child before this call returns, and nothing comes back to the parent.
---
--- WHAT COMES BACK is an array aligned with `children` by index. A slot is
--- `{ ok = true, values = <the body's returns, packed> }` or `{ ok = false,
--- err = <the refusal's error table, or whatever the body raised> }`, and it
--- is never nil: a caller reads position 2 to find out about the second
--- sibling, whatever happened to it.
---
--- THE DEFAULT IS ISOLATE. One sibling failing cancels nobody: the others run
--- to completion and their slots say so. `opts.joiner = "cancel_on_error"`
--- cancels the scope at the first failure instead, and the siblings that were
--- still running unwind at their next checkpoint — their slots carry the
--- cancellation and `cancelled = true`. Cancellation is cooperative: a sibling
--- that never reaches a checkpoint is not stopped by it, which is what
--- `opts.timeout_ms` is for.
---
--- HOW A CHILD CLOSES, in three layers, outermost last:
---
---   1 the bracket. `child` closes explicitly on the way out — the normal path
---     and the raising one alike, including the raise a cancel makes;
---   2 the unwinding. A cancel reaching a checkpoint inside a body is a raise
---     like any other, so layer 1 runs on the way through: `<close>` and the
---     bracket's pcall are the same mechanism a body error uses;
---   3 the kernel's Drop backstop. A hard abort (a `with_timeout` whose grace
---     ran out) drops the task mid-await and no Lua runs at all — the handle's
---     Drop records `session_closed{ reason = "dropped" }`, which is why a
---     supervisor that lost a child still leaves a closed boundary in the log.
---
--- ONE DATABASE, AND WHAT SIMULTANEOUS WRITERS MEET THERE. Siblings are
--- children of one parent, so they write to the parent's database, and the
--- store decides what that costs. On a FILE database, two writers contend and
--- the kernel's busy timeout waits it out. On the IN-MEMORY database the
--- kernel addresses by a shared-cache URI, shared cache locks per TABLE and a
--- second writer gets SQLITE_LOCKED immediately — which the busy timeout does
--- not cover — so a sibling's beat can come back `Outcome.err("state")` with
--- `detail.kind == "busy"` [実測: 2026-09-05, crates/agent-block/tests/
--- fixtures/knl_beat_test.lua inv15: the same two children fail
--- nondeterministically on the in-memory store and pass on a file one].
---
--- Nothing here retries it. `busy` is the one class the kernel calls
--- retryable, and asking again is the caller's loop's decision — how many
--- times and for how long is what only the loop knows (`policy.retry`, and
--- knl's header on the same point). A supervisor that quietly retried would be
--- the loop this module says it does not have.
---
--- `opts.timeout_ms` wraps the scope in `task.with_timeout`, which cancels at
--- the deadline and hard-aborts anything still alive after the grace window.
--- The deadline does not raise out of this call: a slot no sibling reached is
--- filled with the timeout's own error and `cancelled = true`, because a
--- caller that asked for results by index should not have to catch to read
--- them.
---
--- @param parent userdata|table  the session the children are opened from
--- @param children table  array of { opts = <child opts>, fn = fn(child) }
--- @param opts table|nil  { joiner? = "isolate" | "cancel_on_error", timeout_ms? }
--- @return table results  one slot per entry, aligned by index
function M.parallel(parent, children, opts)
    if not is_handle(parent) then
        error("supervisor.parallel: the first argument must be a knl session", 2)
    end
    if type(children) ~= "table" then
        error("supervisor.parallel: the second argument must be an array of { opts, fn }", 2)
    end
    for i, entry in ipairs(children) do
        local who = "supervisor.parallel: children[" .. i .. "]"
        if type(entry) ~= "table" then
            error(who .. " must be a table { opts, fn }", 2)
        end
        only(entry, { opts = true, fn = true }, who)
        if type(entry.fn) ~= "function" then
            error(who .. ": fn must be a function fn(child)", 2)
        end
        child_contract(entry.opts, who)
    end
    opts = opts or {}
    if type(opts) ~= "table" then
        error("supervisor.parallel: opts must be a table", 2)
    end
    only(opts, { joiner = true, timeout_ms = true }, "supervisor.parallel")
    if opts.joiner ~= nil and opts.joiner ~= "isolate" and opts.joiner ~= "cancel_on_error" then
        error("supervisor.parallel: joiner must be 'isolate' or 'cancel_on_error', got " .. tostring(opts.joiner), 2)
    end
    if opts.timeout_ms ~= nil and not whole_at_least(opts.timeout_ms, 1) then
        error("supervisor.parallel: timeout_ms must be a whole number >= 1, got " .. tostring(opts.timeout_ms), 2)
    end
    shape.assert_dev(opts, PARALLEL_OPTS, "supervisor.parallel opts")

    -- Everything above is refused before a single child is opened: a list with
    -- a bad fifth entry must not leave four sessions in the log.
    local task = nursery()
    local cancel_on_error = opts.joiner == "cancel_on_error"
    local results = {}

    local function body(scope)
        for i, entry in ipairs(children) do
            scope:spawn(function()
                local status, payload = attempt(parent, entry.opts, entry.fn)
                if status == "ran" then
                    results[i] = { ok = true, values = table.pack(table.unpack(payload, 2, payload.n)) }
                    return
                end
                local slot = { ok = false, err = payload }
                if status == "raised" and is_cancellation(payload) then
                    slot.cancelled = true
                end
                results[i] = slot
                if cancel_on_error then
                    scope:cancel()
                end
            end)
        end
    end

    -- The scope waits for every child it spawned before it returns, so the
    -- slots are complete by the line below — unless the deadline tore the
    -- scope down, which is the one path that raises out of the nursery.
    local ran, raised
    if opts.timeout_ms ~= nil then
        ran, raised = pcall(task.with_timeout, opts.timeout_ms, body)
    else
        ran, raised = pcall(task.scope, body)
    end

    for i = 1, #children do
        if results[i] == nil then
            results[i] = {
                ok = false,
                err = ran and "the scope ended before this child finished" or raised,
                cancelled = true,
            }
        end
    end
    return results
end

-- ============================================================
-- merge — several sessions read into one request
-- ============================================================

--- The one statement `merge` runs: the events of the named streams, in stream
--- and seq order.
---
--- Four columns and no more, because a fold reads four things: what kind the
--- event is, what it is about (`data`), which stream it belongs to, and the
--- order it was written in. `stream` and `seq` never reach the fold — they are
--- how the rows are bucketed and ordered before it.
---
--- `$sessions` is the set, expanded by the kernel into one bound placeholder
--- per id. No value is concatenated into the text, exactly as in `knl.views`.
local MERGE_SQL = [[
SELECT stream,
       seq,
       kind,
       data
  FROM events
 WHERE stream IN $sessions
 ORDER BY stream, seq
]]

--- A row's `data`, as the fold wants it.
---
--- The column is TEXT holding JSON — a query answers what SQLite holds, and
--- the decoding `session:events()` does on the way out of the syscall is not
--- done for a read. A NULL column (an event that carried no `data`) is nil,
--- which is what `knl.fold` already reads as "nothing this fold is about".
local function decoded(decode, value)
    if value == nil then
        return nil
    end
    if type(value) == "table" then
        return value
    end
    return decode(value)
end

--- A session, as the id a query binds: a string is one already, a handle is
--- asked for its own.
local function stream_id(value, who)
    if type(value) == "string" then
        return value
    end
    if is_handle(value) then
        local ok, id = pcall(function()
            return value:id()
        end)
        if ok and type(id) == "string" then
            return id
        end
    end
    error(who .. ": a session must be its id, or a knl session handle that answers one", 3)
end

--- Build a `fold` that reads `sessions` and the folding session into ONE
--- request.
---
---     local device = knl.device({
---         llm = llm,
---         fold = supervisor.merge(parent, { first, second }),
---     })
---     local outcome = knl.beat(parent, device)
---
--- What it answers is the concatenation, IN THE ORDER GIVEN, of each listed
--- session's events folded by the kernel's default fold, followed by the
--- parent's own. Each session is folded separately rather than merged into one
--- event list first, and that is not a detail: `knl.fold` pairs an assistant
--- message's `tool_use` ids against the `tool_result`s that answered them, and
--- interleaving two logs would let one session's results close the other's
--- calls.
---
--- THE ORDER IS A CHOICE. "The order given, children first, the parent last"
--- is what a supervisor handing work out and reading it back wants, and it is
--- the first thing the first real need may change — an interleaving by
--- `epoch_ms`, say, or the parent's seed in front. It is written here, once, so
--- there is something to change.
---
--- Ownership never moves. Nothing is appended to any stream: the children's
--- histories are READ, and the request that comes out of this fold is recorded
--- as the parent's own `llm_request` like any other.
---
--- A read that hits the row cap raises rather than folding what it got. A
--- request quietly missing the middle of a child's history is the one failure
--- that would look exactly like the child having said less.
---
--- @param parent userdata|table  the session the read runs on (the folding one)
--- @param sessions table  the streams to read, in the order they belong in
--- @param opts table|nil  { limit?, timeout_ms? }
--- @return function fold  fn(events, device) -> request
function M.merge(parent, sessions, opts)
    if not is_handle(parent) then
        error("supervisor.merge: the first argument must be a knl session", 2)
    end
    if type(sessions) ~= "table" or #sessions == 0 then
        error("supervisor.merge: the second argument must be a non-empty array of sessions", 2)
    end
    opts = opts or {}
    if type(opts) ~= "table" then
        error("supervisor.merge: opts must be a table", 2)
    end
    only(opts, { limit = true, timeout_ms = true }, "supervisor.merge")
    for _, name in ipairs({ "limit", "timeout_ms" }) do
        if opts[name] ~= nil and not whole_at_least(opts[name], 1) then
            error("supervisor.merge: " .. name .. " must be a whole number >= 1, got " .. tostring(opts[name]), 2)
        end
    end
    shape.assert_dev(opts, MERGE_OPTS, "supervisor.merge opts")

    local ids = {}
    for i, value in ipairs(sessions) do
        ids[i] = stream_id(value, "supervisor.merge: sessions[" .. i .. "]")
    end
    -- Frozen at construction, like a device's fields: the decoder this needs
    -- and the set it reads are what this fold IS, and a fold that resolved
    -- either per beat could answer two different things for one device.
    local decode = json_decoder()
    local query_opts = { sessions = ids, limit = opts.limit, timeout_ms = opts.timeout_ms }

    return function(events, device)
        local rows, truncated = parent:query(MERGE_SQL, nil, query_opts)
        if truncated then
            error("supervisor.merge: the read hit the row limit — the request would be missing history", 0)
        end

        local by_stream = {}
        for _, row in ipairs(rows) do
            local bucket = by_stream[row.stream]
            if bucket == nil then
                bucket = {}
                by_stream[row.stream] = bucket
            end
            bucket[#bucket + 1] = { kind = row.kind, data = decoded(decode, row.data) }
        end

        local messages = tag_array({})
        for _, id in ipairs(ids) do
            for _, message in ipairs(kernel.fold(by_stream[id] or {}, device).messages) do
                messages[#messages + 1] = message
            end
        end

        -- The parent's own log, folded by the kernel — which is also where
        -- `system` and `tools` come from, composed from the device the way
        -- every other fold composes them.
        local request = kernel.fold(events, device)
        for _, message in ipairs(request.messages) do
            messages[#messages + 1] = message
        end
        request.messages = messages
        return request
    end
end

-- ============================================================
-- The API registry
-- ============================================================
--
-- One entry per public export, naming the shape of what goes in and what comes
-- out — the same form `knl.shapes.api` and `policy.shapes.api` use.
--
-- `args` is an ordered list of `{ shape, desc }`, one per positional argument,
-- and it is what the dev-mode gate below RUNS. `members` names the functions a
-- caller SUPPLIES or an export hands BACK: they are not exports of this module,
-- so their entries are declared and walked by the spec and nothing wraps them.

--- One declared argument: the shape it is held to, and the word for it.
local function arg_of(schema, desc)
    return { shape = schema, desc = desc }
end

local PARENT_ARG = arg_of(SESSION_HANDLE, "parent")
local BODY_ARG = arg_of(FUNCTION, "fn(child)")

-- The `*_ARG` shapes are the OPEN twins of the published contracts (see
-- `opts_contract`): the registry holds a call to the SHAPE of the options it
-- declared, and whether a key is declared at all is `only`'s judgement, made in
-- both modes.

M.shapes.api = {
    child = {
        args = { PARENT_ARG, arg_of(CHILD_OPTS_ARG, "opts"), BODY_ARG },
        returns = "whatever fn returned — or nil, knl.shapes.error (kind 'refused') when the allocation was refused",
        members = {
            fn = {
                args = { arg_of(SESSION_HANDLE, "child") },
                returns = "anything; the bracket hands it back unchanged",
            },
        },
    },
    parallel = {
        args = {
            PARENT_ARG,
            arg_of(T.array_of(CHILD_ENTRY), "children"),
            arg_of(PARALLEL_OPTS_ARG, "opts?"),
        },
        returns = RESULTS,
        members = {
            fn = {
                args = { arg_of(SESSION_HANDLE, "child") },
                returns = "anything; it lands in that sibling's slot, packed",
            },
        },
    },
    merge = {
        args = {
            PARENT_ARG,
            arg_of(T.array_of(MERGE_SESSION), "sessions"),
            arg_of(MERGE_OPTS_ARG, "opts?"),
        },
        returns = "fold — fn(events, device) -> request",
        members = {
            fold = {
                args = {
                    arg_of(T.array_of(kernel.shapes.event_base), "events (the folding session's own)"),
                    arg_of(T.table, "device (read for system / tools)"),
                },
                returns = kernel.shapes.request,
            },
        },
    },
    shapes = {
        args = {},
        returns = "this registry: every shape above, plus `api`",
    },
}

-- ============================================================
-- The registry, executed
-- ============================================================
--
-- In dev mode each declared export is replaced, once, here at load, by a
-- wrapper that holds the call to its entry. Prod installs nothing and a call
-- pays nothing — which is why every check a call must not get through without
-- is written beside it, loud in both modes.
--
-- What the gate judges is the shape of the arguments that were PASSED. An
-- argument nobody supplied is left to the function, so `supervisor.parallel(p,
-- entries)` still reaches its own defaulting rather than a shape violation
-- about an opts table that was never there.

local function arg_checked(name, fn, declared)
    return function(...)
        for i = 1, #declared do
            local value = select(i, ...)
            if value ~= nil then
                shape.assert_dev(value, declared[i].shape, name .. " arg " .. i .. " (" .. declared[i].desc .. ")")
            end
        end
        return fn(...)
    end
end

if shape.is_dev_mode() then
    for name, entry in pairs(M.shapes.api) do
        local export = M[name]
        if type(export) == "function" then
            M[name] = arg_checked("supervisor." .. name, export, entry.args)
        end
    end
end

return M
