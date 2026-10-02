--- knl — the Lua kernel: session + device, the beat, Outcome, shapes, views.
---
--- What this is
---   The driving half of the kernel/shell split. Rust is the pure syscall
---   layer (session / append / events / view / query / reserve / spend /
---   close) and states the kernel's own invariants in its module doc; this
---   module runs a BEAT — one model call plus the tools that call asks for —
---   over that layer and hands back an `Outcome`. There is no loop here: a
---   caller composes beats on the spot, which is why the primitive is one
---   beat and not a run.
---
--- Two arguments, two owners
---   `knl.open{ owner?, budget?, store? }` hands back the kernel's session
---   userdata verbatim — the durable half: the fact-log and the quota, owned
---   by the kernel, advanced only by appending. `knl.device{ llm?, tools?,
---   tool_policy?, fold?, filters?, system?, cost? }` builds the policy half
---   — a stateless value whose defaults are resolved once at construction
---   and then frozen. `knl.beat(session, device)` takes both. They are not
---   bundled into one handle because they differ in owner (kernel / caller),
---   lifetime (durable / per-process) and mutability (append-only / frozen),
---   and the config is CONSUMED at construction rather than carried: a
---   device holds resolved fields, never the table it was configured with.
---   Per-beat policy variation is not an override argument: derive another
---   device with `d:with{ llm = strong }` and beat with that one.
---
--- Where the log lives
---   `store` is optional and leaving it out is the answer for a real session:
---   the log goes into the database the host owns, one file per project. Every
---   session a script opens is a stream in that one file, so a tree opened from
---   a default parent is a tree in one database, and `knl.resume{ session = id }`
---   reopens it by id alone.
---
---   Many sessions in one file is the shape, so what tells them apart is a
---   label and not a file: `knl.open{ meta = { run = id } }` writes the
---   caller's labels on the opening, and a supervisor selects on them. Giving
---   each run a database of its own answers the same question by breaking the
---   one above it — the project's log stops being one stream to read.
---
---   `store = "mem"` is the other choice and has to be asked for by name: an
---   in-memory log, for TESTS AND MOCKS — one per host process, gone with it,
---   never a lighter version of the default. It is the same event log the
---   file is (one writer thread, the same schema), so a `mem` parent can open
---   children and a `mem` stream can be resumed by name while the host lives;
---   what it cannot do is outlive the process.
---   `store = { sqlite = <path> }` is a file the caller picked.
---
--- Lifecycle belongs to the session
---   The canonical bracket is the callback form — the kernel opens, runs the
---   body, and closes, so an error escaping the body still records the
---   boundary:
---
---       local result = knl.session({ owner = "u", budget = { amount = 10 } },
---           function(s) return knl.beat(s, d) end)
---
---   `knl.session` resumes instead of opening when the opts name a session.
---   `local s <close> = knl.open{...}` is the alternative and the Rust Drop
---   backstop is the last resort; a body error always wins over a close that
---   failed on its way out (the suppressed-exception rule, and the loser is
---   logged as a record rather than dropped).
---
--- Beats are declared, not numbered
---   The kernel does not count beats. `knl.beat` mints one id per beat with
---   `knl.new_beat_id()` (time-ordered, session-free) and stamps it on every
---   event that beat writes — llm_request, llm_response, the tool pair, and
---   a failed call's note. It is stamped as `meta.beat`, a label in the
---   envelope like any other; grouping and ordering read it back, nothing
---   more. `resp.beat` carries the same id out to the caller.
---
--- The steps of a beat
---   [0]   the gate: a knl session first, a knl device second, an `llm` on
---         the device — any of the three missing is `Outcome.err("conf")`
---   [0.5] the beat names itself (`knl.new_beat_id`)
---   [1]   request = `device.fold(session:events(), device)` — a read that
---         hit the row cap is refused here rather than folded. A fold may
---         answer a report beside the request (see [4])
---   [2]   the filter chain, each `fn(request) -> request`
---   [3]   `session:reserve(device.cost(request))` — the beat's whole
---         deduction, taken before the call; a refusal stops here, with
---         nothing recorded and no call made
---   [4]   append `llm_request`, carrying what the fold left out as
---         `data.window` when it reported any (`knl.shapes.window_report`)
---   [5]   `device.llm(request)`
---   [6]   append `llm_response`, or `llm_call_failed` with the failure's
---         classification on it
---   [7]   the tools that response asked for: `tool_call` / `tool_result`
---
--- Outcome — the four statuses
---   A beat answers with a value, never a raise, and the value is one of
---   four tagged tables. `Outcome.match(o, arms)` is exhaustive: all four
---   arms are required, and an unknown status is a loud error — the
---   dynamic-language stand-in for a compiler's exhaustiveness check.
---
---     ok       the beat ran. `out` is the call's answer (content / usage /
---              stop_reason / finish_reason?) plus the `beat` id and the tool summary
---     refused  the model answered and declined to make progress. `reason`
---              is the adapter's provider-neutral classification of the
---              refusal ("model" / "content_filter") and `detail` is the
---              whole answer, so a caller can inspect what came back
---     error    the mechanism failed: the beat did not come off
---     stopped  the beat stopped ON PURPOSE before calling — the quota
---              would not cover it. Nothing broke and the model was never
---              asked; this is the branch a caller's loop exits on, and
---              `tag` names the grant that stopped it
---
---   `Outcome.err(kind, detail)` carries TWO vocabularies and they answer
---   different questions. `kind` names the STAGE that failed — "conf" (the
---   device or a caller function it holds), "filter", "call" (the llm), or
---   "state" (a record that could not be laid down). What the failure WAS,
---   and whether asking again could work, is in `detail`, and the two stages
---   that can answer that do:
---
---     "state"  the kernel's own reading — `detail.kind` one of
---              `knl.shapes.error_kinds`, `detail.retryable`
---     "call"   the call's classification — `detail.kind` one of
---              `knl.shapes.call_error_kinds`, `detail.retryable`, and
---              `detail.retry_after` when the provider named one
---
---   A loop deciding on a retry reads `detail.retryable` (or names
---   `detail.kind`s) and never the stage: "state" and "call" say where, not
---   whether. The two vocabularies are separate on purpose — a contended
---   store and a rate-limited provider are not the same failure — and
---   `detail.kind` is the field both answer in, so one predicate reads both.
---
--- What a stored event looks like
---   One envelope, one place for structure. An event is `{ kind, meta?,
---   data? }` and nothing else at the top level — a stray key is
---   refused, not stored — and the kernel stamps `seq` / `epoch_ms` /
---   `_schema_version` on it and keeps the envelope as columns (stream,
---   seq, epoch_ms, kind, schema_version, meta, data).
---
---   The envelope is the half that does not move. `meta` is a SHALLOW map of
---   labels (string / number / boolean values; a nested one is refused) and
---   it is the envelope's correlation bag: the beat id this layer stamps on
---   the events of one beat lives there, as `meta.beat`, and not as a field
---   of its own. Everything a kind is actually ABOUT is structured JSON
---   under `data`, whose shape
---   belongs to whoever writes that kind. The kernel validates the envelope
---   and the `data` of its own kinds (`session_*`, `budget_*`) and nothing
---   else; the shapes for the kinds a beat writes are declared here, in
---   `knl.shapes.events`, and asserted in dev mode at the append sites.
---
---   The point of the split is where a change can hurt. A view that reads
---   the envelope or `meta` is untouched by a schema change; a view that
---   reads a path inside `data` changes together with the shape of the kind
---   it reads — `knl.views.usage` with `llm_response`, `tool_pairs` with the
---   tool pair, `ledger` with `budget_*`. A caller's seed is a kind like any
---   other and goes in the same envelope:
---
---       s:append{ kind = "msg_user", data = { content = "hi" } }
---       s:append{ kind = "msg_user", meta = { label = "seed" },
---                 data = { content = "hi" } }
---
--- Reading the log back: two tiers, and only two
---   BUILT-IN VIEW — `session:view(name, opts?)`, plus `session:events(from)`
---   beside it. These are the kernel's own reads, they are fixed, and they
---   do not grow: the record from a position on, and `tail` (the last `n`
---   events, verbatim). A fold whose consumer is not fixed in kernel terms
---   never becomes a name here.
---
---   BOTH ARE BOUNDED. `tail` reads `n` rows from the end, and `events`
---   answers `rows, truncated` — at most the kernel's row cap, the same one
---   `query` uses — because a stream grows and every row of a read is decoded
---   and built into a Lua table on the VM's thread. `truncated` is the fact a
---   caller cannot infer: it says the cap cut the read short and the rest is
---   further on, read by paging `from`. `knl.beat` refuses one rather than
---   folding a record whose newest events are missing.
---
---   QUERY VIEW — a named Lua function that runs ONE `SELECT`. The log is a
---   SQLite table whose columns the kernel publishes (`knl.shapes.schema`),
---   and `session:query(sql, params?, opts?)` binds values, refuses anything
---   that is not one SELECT / WITH, and resolves `$stream` (this session)
---   and `$sessions` (`opts.sessions`, the set to read across). That is the
---   whole mechanism: no builder, no query object, no registration hook.
---   `knl.views.beats` / `tool_pairs` / `ledger` / `usage` / `sessions` /
---   `tree` are the six this module ships, and a consumer's own view is a
---   function of exactly the same form — nothing about the six is privileged.
---
---   `knl.export(session, opts?)` is neither a third tier nor a view: it is
---   the FIRST tier, paged to the end, plus the two readings a consumer
---   outside this repo actually asks for — the stored events whole
---   (`as = "events"`) or the conversation they carry (`as = "messages"`).
---   It is here rather than in every consumer because the paging is the part
---   that gets left out, and a log read once is a log that ends where the row
---   cap did. Nothing about it depends on the table's columns.
---
---   Token usage is a query view and not a built-in one, deliberately.
---   Every `llm_response` carries the counts its adapter normalized out of
---   the provider's answer, so `knl.views.usage` is an accounting of facts
---   already in the log, in the shell's vocabulary rather than the kernel's
---   — and it is a reading apart from the budget, which is a quota the owner
---   granted and never a tally of tokens (see below).
---
--- The budget
---   The budget is a quota the owner granted the session, not a tally of
---   what it used. beat asks for permission BEFORE it calls —
---   `session:reserve(n)`, after the request is known and before anything is
---   recorded — and a refusal is a planned stop, not a failure and not a
---   model decision: `Outcome.stopped("budget", tag)`, with no `llm_request`
---   event and no call. How much a beat asks for is the device's policy:
---   `device.cost(request)`, one unit per beat by default. What a unit
---   *means* is whatever the owner tagged the grant with — the kernel reads
---   the number and nothing else, and token usage (`knl.views.usage`) is a
---   separate reading that beat never folds back into the budget.
---
---   `reserve` is the WHOLE of a beat's deduction. The kernel's two moves are
---   independent — `reserve(n)` deducts and refuses when the balance is
---   short, `spend(n)` deducts without asking, and neither holds anything for
---   the other to release — so a beat that called both would deduct twice for
---   one call. beat calls `reserve` and nothing else; a caller that meters
---   what a call really cost does it with `spend` from its own loop, knowing
---   it is a second deduction.
---
--- Sessions opened from sessions
---   `knl.open{ parent = s, budget = { from_parent = n } }` opens a session
---   out of `s`'s balance: `s`'s ledger gains a reservation naming the child,
---   the child's log opens with `parent` recorded on it and `n` of its own,
---   and the two are ONE write on the parent's database. A balance that will
---   not cover it records a refusal on the parent and raises `refused` —
---   nothing is opened, and there is no half-opened session to hand back.
---   Nothing comes back when the child closes: an allocation is a spend, the
---   same way a reservation is.
---
---   The kernel keeps no tree. It records two facts — the parent on the
---   child's opening, the child on the parent's ledger entry — and, when a
---   session closes with children that had not ended, their ids on the
---   boundary (`session_closed.data.open_children`, recorded in the same
---   write; a close is never refused for them). Everything else is a
---   supervisor's: `knl.views.tree` is one recursive SELECT over those
---   fields, and a policy over a subtree is a pack above this module rather
---   than a rule inside it.
---
--- What a device promises, function by function
---   Every one of these is the caller's code, and beat holds it to a written
---   contract rather than guessing at a return it does not recognise.
---
---     fold(events, device) -> request
---       pure; the default (`knl.fold`) folds the log into the neutral
---       content-block shape. A raise is `err("conf")`.
---     filter(request) -> request
---       each filter replaces the request wholesale. Returning a non-table
---       is `err("filter")` — loudly, in prod as well as dev, because the
---       value goes into the durable record and onto the wire.
---     cost(request) -> integer >= 1
---       what the beat reserves. Checked in prod, not only asserted in dev:
---       a beat that could ask for zero is how a run stops being finite.
---       The default answers 1, so a grant counts beats.
---     llm(request) -> llm_result | nil, err
---       `knl.shapes.llm_result`: `status` is "ok" or "refused" and there is
---       no third value; `content` is an array of blocks, `usage` is three
---       counts, and a "refused" answer names the refusal's `kind`. A
---       transport or provider failure is `nil, err` (or a raise), which
---       beat records as `llm_call_failed` and reports as `err("call")`.
---       `err` is a `knl.shapes.call_error` when the port classified it —
---       `{ kind, retryable, retry_after?, message, status? }`, which is what
---       lets a retry policy decide — and anything else (a sentence, a raise)
---       is carried as `unknown` and not retryable.
---     tool_policy(tool_use_block, out) -> decision, reason?
---       `nil` (no opinion — run), `"run"` or `"deny"`, and nothing else: a
---       fourth word is a device-contract violation, not a fourth meaning.
---       A policy that RAISES denies — a gate written to veto tools must not
---       fall open on its own bug — and its message becomes the reason.
---
--- When something raises
---   Two kinds of failure meet inside a beat and they are reported
---   differently. A KERNEL SYSCALL raises attributed text — `knl:
---   <method>: <kind>: <message>` — because mlua cannot carry a table out
---   of a Rust callback; beat reads it back with `knl.error` and the
---   reading is what lands in `Outcome.err("state").detail`: `{ kind?,
---   method?, retryable, message }`, with `kind` one of
---   `knl.shapes.error_kinds` and `retryable` true for exactly one of them.
---   beat never acts on `retryable` itself — asking again is the caller's
---   loop's decision, because only the loop knows how many times and for
---   how long it may. A CALLER'S OWN FUNCTION raising (fold, a filter,
---   cost, the llm) is a bug in the device rather than a class of kernel
---   failure, so its detail is the message it raised — plus, in dev mode
---   only, the `traceback` of where it raised.
---
--- knl.shapes is a registry that is EXECUTED
---   Every public interface of this module is declared as an lshape and
---   published through `knl.shapes`, so a caller reads the contract as data
---   rather than out of prose. `knl.shapes.api` goes one further: it names
---   the shape of every argument of every export, and in dev mode each
---   declared export is wrapped once, at load, by a gate that holds the call
---   to its entry. A registry nobody runs is prose with a table around it.
---   Prod installs no wrapper and pays nothing, which is why the checks a
---   call must not get through WITHOUT — a device's config, a filter's
---   return, cost's bound — stay where they are and stay loud in both modes.
---   `knl/spec/api_spec.lua` closes the loop from the other side: an export
---   with no entry, an entry with no export, and a device field
---   `device_config` does not describe are each a failure.
---
---   The registry is written in a file of its own, `knl/shapes.tl`, required
---   as `knl.shapes` and published here unchanged as `M.shapes`. It is data —
---   it builds shapes and reads nothing of this module's — so it sits beside
---   the code rather than in the middle of it; the gates that hold values to
---   it (the append and Outcome asserts, the dev-mode wrapper) stay here, with
---   the code they guard. `agent-block vendor knl` writes the two files
---   together, as it writes every module with its sub-modules.
---
---   Two halves of one table, and they have different owners. Everything
---   above is THIS module's — the shapes of what `knl.beat` / `knl.device` /
---   the views take, executed by the dev gate at the foot of this file.
---   `knl.shapes.session` and `knl.shapes.module` are the BRIDGE's, and they
---   are not written here at all: they point at `knl_types`, generated at
---   host start from the argument and return types of `bridge/knl.rs`. That
---   surface is checked in Rust, on every call and in both modes, so the
---   session userdata needs no wrapper here — which is just as well, since
---   wrapping it would mean handing out a proxy table in place of the
---   kernel's own value, and `local s <close>` and `open{ parent = s }` both
---   want the userdata itself.
---
--- Clean by construction: no legacy, no fabrication
---   This module and `knl_adapter` are kept free of consumer legacy on
---   purpose, because a compat shim taken in here becomes the kernel's
---   vocabulary forever. Nothing is invented on a caller's behalf: an llm
---   answer with no `usage` is `err("call")` rather than `usage or {}`, a
---   refusal without a `kind` is the same rather than a default word, a
---   third llm status does not exist, a nameless `tool_use` block is the
---   provider breaking its contract rather than a hole to fill with an empty
---   string, and no request field (`max_tokens` and friends) is injected
---   behind the caller's back. There are no compat aliases: a tool entry
---   declares `input_schema`, and nothing else is read in its place.
---
--- Deliberately not here
---   Fork (branching a history), scope trees and sub-scope allowances,
---   parallel tool execution, streaming, and a structured error type for a
---   tool_result beyond the raised string. Each is deferred until a real
---   loop asks for it, not designed ahead of the need.

--- The host's globals, declared: `std`, whose `json` renders a non-string
--- tool result into request text, and `knl`, the syscall bridge captured
--- below. Both are declared in `host_types.d.tl` and reached by requiring
--- it, which every Teal module here does — a module states no `global` of
--- its own, because one declared in a `.tl` that other modules require
--- splits that module's record types (the reasoning is in that file's
--- header). The run-time half is an empty table, so what this costs at load
--- is the same nothing it costs the eleven modules above this one.
require("host_types")

--- The Rust syscall bridge, captured before `require("knl")` shadows the
--- name (see the header). `nil` in a VM that has no bridge (e.g. the pure
--- lspec runner): `fold` / `Outcome` / `device` never touch it, and the
--- entry points that do report its absence rather than indexing nil.
local syscall = knl

--- The bridge's argument and return types, generated by the host from
--- `bridge/knl.rs`. A `local type` import for the same reason as above, and
--- here it is load-bearing: this module runs in VMs that have no
--- `knl_types` at all, and the registry below reaches the module's VALUES
--- through a `pcall(require, ...)` that answers a describing stand-in when
--- there is none. A hard `require` at the top would raise before that
--- fallback could ever be reached.
local type kt = require("knl_types")

local lshape = require("lshape")
local T = lshape.t
local shape = lshape.check

--- The contracts, as data (`knl/shapes.tl`): the value, because the gates
--- below read the shapes at run time, and the record it is declared with,
--- for `M.shapes`. The shapes file requires nothing of this module's.
local shapes = require("knl.shapes")
local type Shapes = require("knl.shapes")

-- ============================================================
-- The declared surface
-- ============================================================
--
-- What used to be `blocks/lib/knl.d.tl` — the records the modules above this
-- one read (supervisor, policy, knl_adapter, the two blocks). They are
-- declared HERE, beside the module table and not inside it, because a record
-- nested in `M` is a KEY of `M` in the generated Lua (`M = { Session = {} }`)
-- and `knl/spec/api_spec.lua` walks this module's keys: every export must
-- have a registry entry, so a type that materialised as an export would fail
-- that walk. `M` aliases each one with `type X = X`, which is a declaration
-- and generates nothing, so `kernel.Session` still names the type from a
-- module that requires this one.
--
-- Where lshape says `discriminated("status", …)` a record here has every
-- arm's fields, because a Teal union of records needs a `where` on each and
-- the readers test `status` and read the field they know follows; the arms
-- are documented on the record.

--- A kernel session: the durable half. What the kernel's session
--- userdata answers (`bridge/knl.rs`); a fake answers the same.
local record Session
    --- The session's id, its scope's, and its owner.
    id: function(Session): kt.SessionId
    scope_id: function(Session): kt.ScopeId
    owner: function(Session): kt.Owner
    --- One record onto the log; the seq it was written at.
    append: function(Session, kt.AppendEvent): kt.Seq
    --- The events from a seq (all when none), and whether the read was
    --- cut at the row cap.
    events: function(Session, ?kt.Seq): {{string:any}}, boolean
    --- How many events the log holds.
    len: function(Session): kt.Count
    --- One of the kernel's views of this session, by name.
    view: function(Session, kt.ViewName, ?kt.ViewOpts): any
    --- Rows of a statement over the log, and whether the read was cut.
    --- The options are this module's `QueryOpts` — the same three fields
    --- the syscall's (`knl_types.QueryOpts`) declares — so a module typed
    --- against `knl` needs no `require("knl_types")` at run time, where
    --- that module is the host's and a bare VM has none.
    query: function(Session, kt.Sql, ?kt.QueryParams, ?QueryOpts): {{string:any}}, boolean
    --- Hold units of the budget: what was granted, and the grant's tag.
    reserve: function(Session, kt.Amount): kt.Amount, string
    --- Spend held units.
    spend: function(Session, kt.Amount)
    remaining: function(Session): kt.Remaining
    exhausted: function(Session): kt.Exhausted
    --- Close the session, saying why.
    close: function(Session, ?kt.CloseReason, ?kt.CloseDetail)
end

--- One tool as a caller writes it (`knl.shapes.tool_spec`).
local record ToolSpec
    name: string
    description: string
    input_schema: any
    handler: function(any): any
end

--- The entry a device's map holds under the tool's name.
local record ToolEntry
    description: string
    input_schema: any
    handler: function(any): any
end

--- What a model call answers (`knl.shapes.llm_usage`).
local record Usage
    input_tokens: number
    output_tokens: number
    thinking_tokens: number
end

--- A model call's answer (`knl.shapes.llm_result`): `status` is `ok` or
--- `refused`; `refusal` is set on the second.
local record LlmResult
    status: string
    content: {any}
    usage: Usage
    stop_reason: string
    finish_reason: string
    refusal: any
end

--- A failed model call, classified (`knl.shapes.call_error`).
local record CallError
    kind: string
    retryable: boolean
    retry_after: number
    message: string
    status: number
end

--- What a fold answers: the request a model call carries.
local record Request
    messages: {any}
    system: any
    tools: {any}
end

--- What a fold answers beside the request when it left something out
--- (`knl.shapes.window_report`).
local record WindowReport
    dropped: {string}
    kept: number
    seed_kept: boolean
    before: number
    after: number
    limit: number
    reserve: number
end

--- The policy half: a frozen value with the resolved fields, and `with`
--- for a derived one.
local record Device
    llm: any
    tools: {string:ToolEntry}
    tool_policy: function(any, any, any): any
    fold: function({any}, Device): Request
    filters: {function(any): any}
    system: any
    cost: function(any): number
    with: function(Device, DeviceConfig): Device
end

--- What `knl.device` takes (`knl.shapes.device_config`).
local record DeviceConfig
    llm: any
    tools: {string:ToolEntry}
    tool_policy: function(any, any, any): any
    fold: function({any}, Device): Request
    filters: {function(any): any}
    system: any
    cost: function(any): number
end

--- What a beat answers (`knl.shapes.outcome`): `status` is one of `ok`
--- (`out`), `refused` (`reason`, `detail`), `error` (`kind`, `detail`),
--- `stopped` (`reason`, `tag`).
local record OutcomeValue
    status: string
    out: any
    reason: string
    detail: any
    kind: string
    tag: string
end

--- The judgements on an outcome, and the exhaustive match — `knl.Outcome`
--- (the record is named apart from the field, which Teal asks for). The
--- four constructors are on it too: they are what a caller building an
--- Outcome of its own reaches for, and what this module builds every one of
--- its own answers with.
local record OutcomeApi
    ok: function(any): OutcomeValue
    refused: function(string, any): OutcomeValue
    err: function(string, any): OutcomeValue
    stopped: function(string, ?string): OutcomeValue
    is_ok: function(any): boolean
    is_refused: function(any): boolean
    is_error: function(any): boolean
    is_stopped: function(any): boolean
    match: function(OutcomeValue, {string:function(OutcomeValue): any}): any
end

--- What `knl.error` reads a raised syscall into (`knl.shapes.error`).
local record Error
    kind: string
    method: string
    retryable: boolean
    message: string
    cause: string
end

--- What `knl.open` takes.
local record OpenOpts
    owner: string
    budget: any
    store: any
    meta: any
    parent: any
end

--- What `knl.resume` takes.
local record ResumeOpts
    store: any
    session: string
    budget: any
end

--- What a view or an export takes: the streams to span, and the read's
--- bounds.
local record QueryOpts
    sessions: {string}
    timeout_ms: number
    limit: number
end

local record ExportOpts
    as: string
    sessions: {string}
end

--- The kernel's views: rows, and whether the read was cut.
local record Views
    beats: function(Session, ?QueryOpts): {{string:any}}, boolean
    tool_pairs: function(Session, ?QueryOpts): {{string:any}}, boolean
    ledger: function(Session, ?QueryOpts): {{string:any}}, boolean
    usage: function(Session, ?QueryOpts): {{string:any}}, boolean
    tree: function(Session, ?QueryOpts): {{string:any}}, boolean
    sessions: function(Session, ?QueryOpts): {{string:any}}, boolean
end

-- `Shapes`, the record `M.shapes` is typed with, is declared with the table
-- that holds it, in `knl/shapes.tl`, and imported at the top of this file.

--- The module, and the whole of what it exports. Every `type X = X` below
--- is a declaration that generates nothing, so this record's KEYS are its
--- exports and nothing else — which is what `knl/spec/api_spec.lua` walks.
local record M
    type Session = Session
    type ToolSpec = ToolSpec
    type ToolEntry = ToolEntry
    type Usage = Usage
    type LlmResult = LlmResult
    type CallError = CallError
    type Request = Request
    type WindowReport = WindowReport
    type DeviceConfig = DeviceConfig
    type Device = Device
    type OutcomeValue = OutcomeValue
    type OutcomeApi = OutcomeApi
    type Error = Error
    type OpenOpts = OpenOpts
    type ResumeOpts = ResumeOpts
    type QueryOpts = QueryOpts
    type ExportOpts = ExportOpts
    type Views = Views
    type Shapes = Shapes

    Outcome: OutcomeApi
    shapes: Shapes
    views: Views

    --- The request a beat sends, folded from the log by the device.
    fold: function({any}, Device): Request, WindowReport
    --- The policy half, resolved once and frozen.
    device: function(DeviceConfig): Device
    --- A session: the kernel's userdata, verbatim.
    open: function(OpenOpts): Session
    resume: function(ResumeOpts): Session
    --- The canonical bracket: open, run `fn`, close — and close on a raise.
    session: function(any, function(Session): any...): any...
    new_beat_id: function(): string
    --- A raised syscall, read back into a table.
    error: function(any): Error
    --- The declared surface, for a tool that wants to read it.
    api: function(): {string:any}
    is_session: function(any): boolean
    --- One model call plus the tools that call asks for, over the session.
    beat: function(Session, Device): OutcomeValue
    --- The events, or the message entries, in seq order.
    export: function(Session, ?ExportOpts): {any}
    _execute_tools: any
end


--- The bridge function `method` names, resolved at call time.
---
--- The load-time capture above is the primary source; the global is read
--- again as a fallback so a spec that installs a fake bridge after this
--- module loaded still reaches it. Missing either way is a loud error, not
--- an index of nil.
local function bridge(method: string): function(...: any): any
    local b = syscall
    if type(b) ~= "table" or type(b[method]) ~= "function" then
        -- The fallback read is the same bare table of syscalls the capture
        -- above is, reached by a name held in a variable.
        b = rawget(_G, "knl") as {string:any}
    end
    if type(b) ~= "table" or type(b[method]) ~= "function" then
        error("knl." .. method .. ": the knl syscall bridge is not available in this VM", 3)
    end
    -- The bridge is a map of syscalls, so what comes off it is `any` and the
    -- thing this returns is called. The four callers name their own return.
    return b[method] as function(...: any): any
end

--- Read a raised kernel failure back as data: `{ kind?, method?, retryable,
--- message }` — the *When something raises* section of the header.
---
--- The bridge cannot raise a table, so it raises the text `knl: <method>:
--- <kind>: <message>` and publishes `knl.error` to read it back. That
--- reader is resolved through the same lazy `bridge()` lookup
--- `new_beat_id` uses, so a spec that installs a fake bridge after this
--- module loaded still reaches it.
---
--- A VM with no bridge at all (the pure lspec runner) gets the same fields
--- with nothing read out of them, so `Outcome.err("state").detail` has one
--- shape everywhere: unclassified (`kind` and `method` absent), not
--- retryable, and the raised text verbatim. That is also what an
--- unattributed raise gets from the bridge itself — a reader that raised on
--- unfamiliar input would make a second failure inside the handler for the
--- first.
---
--- @param e any  the value a pcall'd syscall raised
--- @return table  { kind?, method?, retryable, message }
local function read_error(e: any): Error
    local reader_ok, reader = pcall(bridge, "error")
    if reader_ok then
        -- `pcall` answers `any`, and what came back is the reader this
        -- module just resolved: a function of the raised value.
        local read_ok, structured = pcall((reader as function(any): any), e)
        if read_ok and type(structured) == "table" then
            return structured as Error
        end
    end
    return { retryable = false, message = tostring(e) }
end

-- ============================================================
-- Outcome — the result type of a beat
-- ============================================================
--
-- Plain-data status tag tables, never metatable methods: an Outcome crosses
-- the JSON boundary with the kernel, and a metatable does not survive the
-- round trip. Predicates and the match are free functions for the same
-- reason — the value carries data, the module carries behaviour.

local Outcome: OutcomeApi = {}

--- The status values the kernel knows. Exactly these four, provider-neutral.
local STATUSES: {string} = { "ok", "refused", "error", "stopped" }

--- A beat that ran: `out` is the call's return (content / usage /
--- stop_reason, plus the `beat` id and the tool summary this layer added).
function Outcome.ok(out: any): OutcomeValue
    return { status = "ok", out = out }
end

--- The model produced a response but refused to make progress. `detail`
--- carries the call's `out` so a caller can inspect what came back.
function Outcome.refused(reason: string, detail: any): OutcomeValue
    return { status = "refused", reason = reason, detail = detail }
end

--- The beat did not come off: the mechanism failed. `kind` is one of the
--- kernel's own failure points — "conf", "filter", "call" or "state" (a
--- record that could not be laid down: closed session, a store that would
--- not take the write, event validation).
---
--- Two vocabularies meet on this value and they answer different questions.
--- `kind` here names the STAGE of the beat that failed. What the failure
--- *was* — and whether asking again could work — is in `detail`: a "state"
--- failure carries the kernel's reading (`detail.kind` one of
--- `knl.shapes.error_kinds`, `detail.retryable`). A loop deciding on a retry
--- reads `detail.retryable`, never this `kind`; "state" says where, not
--- whether.
function Outcome.err(kind: string, detail: any): OutcomeValue
    return { status = "error", kind = kind, detail = detail }
end

--- The beat stopped on purpose before calling: an allowance would not cover
--- it. Not a failure (nothing broke) and not a refusal (the model was never
--- asked) — the branch a caller's loop exits on. `tag` names the grant that
--- stopped it, when the owner gave it one.
function Outcome.stopped(reason: string, tag?: string): OutcomeValue
    return { status = "stopped", reason = reason, tag = tag }
end

function Outcome.is_ok(o: any): boolean
    -- A predicate takes whatever a caller holds, so the status is read off
    -- the Outcome shape it is being tested against.
    return type(o) == "table" and (o as OutcomeValue).status == "ok"
end

function Outcome.is_refused(o: any): boolean
    -- A predicate takes whatever a caller holds, so the status is read off
    -- the Outcome shape it is being tested against.
    return type(o) == "table" and (o as OutcomeValue).status == "refused"
end

function Outcome.is_error(o: any): boolean
    -- A predicate takes whatever a caller holds, so the status is read off
    -- the Outcome shape it is being tested against.
    return type(o) == "table" and (o as OutcomeValue).status == "error"
end

function Outcome.is_stopped(o: any): boolean
    -- A predicate takes whatever a caller holds, so the status is read off
    -- the Outcome shape it is being tested against.
    return type(o) == "table" and (o as OutcomeValue).status == "stopped"
end

--- Match an Outcome against `arms = { ok = fn, refused = fn, error = fn,
--- stopped = fn }`.
---
--- Exhaustive: every one of the four arms must be present, and the actual
--- status must be one the kernel knows. A missing arm is a loud error rather
--- than a silently-dropped case (the dynamic-language stand-in for a
--- compiler's exhaustiveness check).
function Outcome.match(o: OutcomeValue, arms: {string:function(OutcomeValue): any}): any
    if type(arms) ~= "table" then
        error("Outcome.match: arms must be a table")
    end
    for _, status in ipairs(STATUSES) do
        if type(arms[status]) ~= "function" then
            error("Outcome.match: missing arm for '" .. status .. "'")
        end
    end
    local arm = arms[o.status]
    if arm == nil then
        error("Outcome.match: unknown status '" .. tostring(o.status) .. "'")
    end
    return arm(o)
end

M.Outcome = Outcome

-- ============================================================
-- fold — events -> request (pure)
-- ============================================================

--- The JSON-array tag the bridge's converter honours (`lua_to_json` reads
--- `__jsontype = "array"`).  Every array fold builds is tagged, so the empty
--- case crosses the boundary — into the durable `llm_request` event and onto
--- the provider wire — as `[]`, not `{}` (the empty-array boundary class
--- this repo has prior fixes for).
local ARRAY_TAG: {string:any} = { __jsontype = "array" }

local function tag_array(t: {any}): {any}
    -- `__jsontype` is the converter's tag and not a metamethod, so the
    -- table carrying it is cast to the metatable position it is used in.
    return setmetatable(t, ARRAY_TAG as metatable<{any}>)
end

--- Render a tool_result's payload as request text: a string verbatim, any
--- other value JSON-encoded (the envelope already carries the rest).
local function result_text(result: any): string
    if type(result) == "string" then
        return result as string
    end
    return std.json.encode(result)
end

--- A stored `tool_result`'s `data`, as the content block that answers the
--- call it names.
---
--- One place, two readers: the fold builds a request out of these, and
--- `knl.export{ as = "messages" }` builds a reading out of the same events.
--- The alternative was the second reader spelling out `tool_use_id` /
--- `is_error` / the rendering of a non-string result for itself, which is one
--- content shape with two versions waiting to disagree.
---
--- Nothing is cut here, and nothing ever was: a result is rendered whole. A
--- cap on what a tool may ANSWER is `policy.result_cap`, and it acts on the
--- handler's return before the log ever sees it — so what the log holds is
--- what both readers hand on.
local function tool_result_block(data: {string:any}): {string:any}
    return {
        type = "tool_result",
        tool_use_id = data.call_id,
        content = result_text(data.result),
        is_error = (data.ok == false) or nil,
    }
end

--- The wire declarations for `tools` (name -> { description, input_schema,
--- handler }), handler stripped: the request carries what the model may
--- call, not how to call it. Sorted by name so fold stays deterministic
--- (pure), which is what lets a KV cache hold across beats.
local function wire_tools(tools: {string:ToolEntry}): {any}
    local names: {string} = {}
    for name in pairs(tools) do
        names[#names + 1] = name
    end
    table.sort(names)
    local out = tag_array({})
    for _, name in ipairs(names) do
        local spec = tools[name]
        out[#out + 1] = {
            name = name,
            description = spec.description,
            input_schema = spec.input_schema,
        }
    end
    return out
end

--- Fold an event list into a provider-neutral request.
---
--- The default fold, for chat-shaped providers. Pure: it reads `events` and
--- the `device`'s policy fields and writes nothing. Three kinds map to
--- messages and the rest are skipped (tool_call / session_* / budget_* /
--- llm_call_failed / llm_request):
---
---   msg_user     -> { role = "user", content = <data.content verbatim> }
---   llm_response -> { role = "assistant", content = <data.content verbatim> }
---   tool_result  -> collected, in seq order, into the user message that
---                   follows the assistant message they answer (consecutive
---                   tool_results batch together, which for a well-formed
---                   history is the same as grouping by beat)
---
--- What a kind is about lives under `data` (see the header), so that is
--- where this reads: `data.content`, `data.call_id`, `data.ok`,
--- `data.result`. An event that carried none — a caller's own kind, or one
--- of the kernel's own boundaries — falls through the same skip as the rest
--- rather than indexing nil.
---
--- `system` and `tools` are composed from the device each beat, not read
--- from the history.
---
--- What "provider-neutral" names here
---   Neutral is a choice of shape, not the absence of one: the request and
---   response this fold speaks ARE the Anthropic content-block shape. An
---   assistant message is an array of blocks, a `tool_use` block carries a call
---   and a `tool_result` block answers it by `tool_use_id`. `close_dangling`
---   below depends on exactly that — it pairs the `tool_use` ids of an
---   assistant message against the results that answered them, a repair no
---   flatter shape could express. Other providers do not arrive here in
---   their own dialect: `llm_proto`'s adapters normalise them into these
---   blocks on the way in and render them back to the provider's wire on the
---   way out, so fold never sees a provider-specific shape.
---
--- What it is handed, and what it does not check
---   `events` is a WHOLE record, and this function takes that on trust: it is
---   an array, and an array carries no mark saying a read of it stopped early.
---   Whoever read the log knows — `session:events()` answers `truncated`
---   beside the rows — and that is where the refusal is (`knl.beat` [1]),
---   because the callers that hand arrays in are not all reading a session:
---   `supervisor.merge` folds query rows, and a caller's own fold may build
---   the list itself.
---
---   It does not repair content either, beyond closing a dangling `tool_use`.
---   An `llm_response` recorded with an empty `content` array folds to an
---   assistant message whose content is `[]`, verbatim — see the pinned shape
---   in `knl/spec/device_spec.lua`. Nothing here decides whether a provider
---   will take that; a filter that means to drop or fill such a turn is a
---   filter, and it has the request to work on.
---
--- @param events table  array of stored events (from `session:events()`)
--- @param device table  a knl device (any table carrying system / tools)
--- @return table request  { system?, messages, tools? }
function M.fold(events: {any}, device: Device): Request, WindowReport
    device = device or {}
    local messages = tag_array({})
    local batch = tag_array({})

    -- The tool_use ids of the most recent assistant message, in block
    -- order, and which of them a recorded tool_result has answered.  A
    -- history can legitimately end (or be interrupted) between the
    -- llm_response append and the tool results — the state a crash
    -- mid-tool leaves behind — and the provider rejects an assistant
    -- message whose tool_use ids have no answering tool_result.  fold
    -- repairs that read-side: any id still unanswered when the next
    -- message begins (or the history ends) is closed with a synthetic
    -- is_error result, so a resumed stream stays foldable instead of
    -- 400-ing forever.
    -- A `tool_use` id is whatever the provider minted, so the list and the
    -- set of answered ones are keyed by the value itself.
    local pending, answered: {any}, {any:boolean} = {}, {}

    local function close_dangling()
        for _, id in ipairs(pending) do
            if not answered[id] then
                batch[#batch + 1] = {
                    type = "tool_result",
                    tool_use_id = id,
                    content = "tool execution was interrupted before a result was recorded",
                    is_error = true,
                }
            end
        end
        pending, answered = {}, {}
    end

    local function flush()
        if #batch > 0 then
            messages[#messages + 1] = { role = "user", content = batch }
            batch = tag_array({})
        end
    end

    -- A stored event is read by key here rather than as a record: `fold` is
    -- handed arrays that never came from `session:events` at all (a caller's
    -- own fold, `supervisor.merge`), and what it reads off one is four
    -- names.
    for _, ev in ipairs((events or {}) as {{string:any}}) do
        local kind = ev.kind
        -- The kind's own content, all of it in one place. An event with no
        -- `data` reads as an empty one, so a malformed record is skipped
        -- like an unknown kind instead of raising inside a pure function.
        local data = (type(ev.data) == "table" and ev.data or {}) as {string:any}
        if kind == "msg_user" then
            close_dangling()
            flush()
            messages[#messages + 1] = { role = "user", content = data.content }
        elseif kind == "llm_response" then
            close_dangling()
            flush()
            messages[#messages + 1] = { role = "assistant", content = data.content }
            if type(data.content) == "table" then
                -- The content of an `llm_response` is an array of blocks
                -- (the header's *provider-neutral* shape); `data` is a map,
                -- so the array and its blocks are named at the read.
                for _, block in ipairs(data.content as {{string:any}}) do
                    if block.type == "tool_use" and block.id ~= nil then
                        pending[#pending + 1] = block.id
                    end
                end
            end
        elseif kind == "tool_result" then
            if data.call_id ~= nil then
                answered[data.call_id] = true
            end
            batch[#batch + 1] = tool_result_block(data)
        end
        -- everything else (tool_call / session_* / budget_* /
        -- llm_call_failed / llm_request) is not part of a request: skip it.
    end
    close_dangling()
    flush()

    local request: Request = { messages = messages }
    if device.system ~= nil then
        request.system = device.system
    end
    if device.tools ~= nil then
        request.tools = wire_tools(device.tools)
    end
    return request
end

-- ============================================================
-- shapes — the public contracts of this module
-- ============================================================
--
-- The shapes are data and are written in `knl/shapes.tl`: every contract,
-- the closed lists beside them and the registry (`api`). This module
-- publishes that table as `M.shapes`, unchanged, and reads its entries by
-- name where a value is held to one (`shapes.outcome`, `shapes.events`).
-- What is here is the code that does the holding, and the two readings of
-- the shapes that only this code uses.

M.shapes = shapes

--- The one label this layer stamps, held to its type.
---
--- `knl.shapes.event_meta` says every value is a scalar, which a NUMBER
--- satisfies — so the shallow-label rule alone would take a numeric beat id,
--- and a correlation key that is not a string is one no view can group on.
--- Open, because every other label in there is the caller's and is already
--- covered by `event_meta`. Asserted beside it at the append sites
--- (`assert_event_dev`), which is where the kernel's own envelope rules are
--- mirrored.
local EVENT_BEAT_LABEL = T.shape({
    beat = T.string:is_optional(),
})

--- The same list as a set, for the one reader that has to ask "is this word
--- one of ours" about a value that came from somewhere else.
local CALL_ERROR_IS_KIND: {string:boolean} = {}
for _, kind in ipairs(shapes.call_error_kinds) do
    CALL_ERROR_IS_KIND[kind] = true
end

--- Dev-mode gate for an event a beat is about to write. No-op in prod.
---
--- Two halves, and they have different owners. The ENVELOPE rules are the
--- ones the kernel also enforces and this layer mirrors so they fail at the
--- line that wrote them rather than at the syscall: a `kind`, and a `meta`
--- that is shallow — plus the one label this layer stamps, `meta.beat`,
--- held to a string where the shallow rule alone would take any scalar
--- (`EVENT_BEAT_LABEL`; this is what the layer adds to the kernel's own
--- validator). The `data` is
--- this layer's own — `knl.shapes.events` holds the shape of each kind it
--- writes, and the kernel stopped judging them — so an unknown kind is
--- simply not checked here, which is what leaves the vocabulary open.
---
--- It guards only what *beat* writes. A caller's own `session:append` goes
--- straight to the kernel: the session handle is the kernel's, not
--- something this module wraps.
local function assert_event_dev(ev: {string:any}): {string:any}
    shape.assert_dev(ev, shapes.event_base, "knl_event")
    if type(ev) == "table" then
        if ev.meta ~= nil then
            shape.assert_dev(ev.meta, EVENT_BEAT_LABEL, "knl_event meta")
        end
        -- The kind is the caller's own word, and the vocabulary this gate
        -- knows is keyed by it; an unknown one simply finds nothing.
        local declared = shapes.events[ev.kind as string]
        if declared ~= nil then
            shape.assert_dev(ev.data, declared, "knl_event data (" .. tostring(ev.kind) .. ")")
        end
    end
    return ev
end

--- Dev-mode gate for an Outcome on its way out of beat. No-op in prod.
---
--- `knl.shapes.outcome` leaves `detail` as `any` — three of the four error
--- kinds carry a message a person reads — but a "state" detail is a
--- contract: it is the kernel's failure read back, and a loop decides on a
--- retry from its `retryable`. So that one is held to `knl.shapes.error`
--- here, which is where a call site that reported a raw string instead of
--- the reading goes red.
local function emit(o: OutcomeValue): OutcomeValue
    shape.assert_dev(o, shapes.outcome, "knl_outcome")
    if o.status == "error" and o.kind == "state" then
        shape.assert_dev(o.detail, shapes.error, "knl_state_detail")
    end
    return o
end

--- `xpcall`'s message handler for a function the CALLER supplied — fold, a
--- filter, cost, the llm — keeping the raised value and the stack apart.
---
--- `debug.traceback` on its own folds the two into one string, and the
--- message is what a beat reports in prod: a detail that grew a stack
--- traceback behind it would be a different report, not a richer one. So
--- the raise is kept verbatim and the stack rides beside it.
---
--- The stack is captured in dev mode only. It is a development aid — which
--- line of somebody's own fold raised, through the pcall that turned the
--- raise into an Outcome — and a production run should not pay for one on
--- every failure.
---
--- `traced` marks the capture rather than letting the traceback's presence
--- stand for it: the `debug` library is not in every VM this module runs in
--- (mlua's safe stdlib leaves it out), and a detail whose TYPE depended on
--- that would make dev mode mean two different things. Dev mode is one
--- thing — a structured detail — and the stack is a field in it that a VM
--- without `debug` simply cannot fill.
local function traced(raised: any): {string:any}
    if shape.is_dev_mode() then
        -- Level 2: skip this handler, so the top frame is the raise itself.
        local tb: string
        if type(debug) == "table" and type(debug.traceback) == "function" then
            tb = debug.traceback("", 2)
        end
        return { raised = raised, traced = true, traceback = tb }
    end
    return { raised = raised }
end

--- The `detail` for a failure that came out of a caller-supplied function:
--- the message beat reports, and in dev mode the traceback beside it.
---
--- In prod this is exactly the string it has always been. In dev it is a
--- table with the same text under `message`, which is the one place the
--- two modes differ in what a beat returns — deliberately, because a
--- traceback is an answer to "where in my code", a question only the person
--- writing that code is asking.
---
--- @param message string  what beat reports about the failure
--- @param caught table  what `traced` handed back
--- @return string|table detail
local function raised_detail(message: string, caught: any): any
    -- `caught` is the value the xpcall handler put there — the map `traced`
    -- builds — and it is read by key.
    if type(caught) == "table" and (caught as {string:any}).traced then
        return { message = message, traceback = (caught as {string:any}).traceback }
    end
    return message
end

--- The classification a failed model call is reported with — the detail of
--- `Outcome.err("call", …)` and the `data` of the `llm_call_failed` note.
---
--- One shape for every way a call does not come off, because a loop reading
--- the Outcome cannot be asked to tell them apart:
---
---   * the port classified it — `nil, { kind, retryable, … }` — and that
---     reading is carried through as it came. The adapter is where a
---     provider's vocabulary is read, and this layer does not second-guess it;
---   * anything else — a raise, an adapter that broke `llm_result`, a device
---     whose `llm` is not a port at all and answered `nil, "..."` — is
---     `unknown` and not retryable. That is the honest reading: beat was not
---     told what kind of failure it was, and inventing one (a "transport" for
---     every message with "timeout" in it, say) would be this layer guessing
---     at the vocabulary it just refused to let the shim hold.
---
--- The dev-mode traceback of a caller's own raising `llm` rides along beside
--- the classification, exactly as `raised_detail` carries it for the stages
--- whose detail is a sentence.
---
--- @param err any  what the llm answered as its error, or the raised value
--- @param caught table|nil  what `traced` handed back, when it raised
--- @return table  `knl.shapes.call_error`
local function call_error(err: any, caught: any): {string:any}
    -- The port's own reading is carried through as it came, so the detail is
    -- the map it was, with the three judgements below written over it.
    local detail: {string:any} = {}
    if type(err) == "table" then
        for k, v in pairs(err as {string:any}) do
            detail[k] = v
        end
    end
    -- A word outside the published vocabulary is not adopted: `unknown` is
    -- what the list says an unnamed failure is, and a beat that passed a
    -- foreign word on would be handing a caller's retry policy a `kind` no
    -- declaration covers. The port's own text still says what happened.
    if not CALL_ERROR_IS_KIND[detail.kind as string] then
        detail.kind = "unknown"
    end
    if type(detail.retryable) ~= "boolean" then
        -- The port answers this itself; for everything else the vocabulary's
        -- own table does, and `unknown` is not retryable.
        detail.retryable = shapes.call_error_retryable[detail.kind as string] == true
    end
    if type(detail.message) ~= "string" then
        detail.message = tostring(err)
    end
    if type(caught) == "table" and (caught as {string:any}).traced then
        detail.traceback = (caught as {string:any}).traceback
    end
    return detail
end

--- What a `traced` failure raised, as text.
local function raised_text(caught: any): string
    if type(caught) == "table" then
        return tostring((caught as {string:any}).raised)
    end
    return tostring(caught)
end

-- ============================================================
-- device — the resolved, frozen policy half
-- ============================================================

--- The fields a device carries. The config is consumed at construction, so
--- this is both the accepted key set and the field set of the result.
--- Everything that names the session itself (owner / store / session /
--- budget) is state and belongs to `knl.open` / `knl.resume`.
local DEVICE_KEYS: {string} = { "llm", "tools", "tool_policy", "fold", "filters", "system", "cost" }

local IS_DEVICE_KEY: {string:boolean} = {}
for _, k in ipairs(DEVICE_KEYS) do
    IS_DEVICE_KEY[k] = true
end

--- The metatable name `beat` recognises a device by. A protected metatable
--- (`__metatable`), so the tag cannot be forged by assignment either.
local DEVICE_TAG = "knl.device"

--- The tag on a device's frozen tools map.
local TOOLS_TAG = "knl.device.tools"

--- How much one beat asks the budget for when the device names no policy.
---
--- One beat, one unit: a beat is the thing being bounded, so the default
--- makes the grant a count of beats. It must be >= 1 — that is what makes
--- the budget a ranking function and the run finite. What the unit *means*
--- is the owner's (`budget = { amount = N, tag = "tokens" }` counts tokens,
--- and then a device supplies `cost` to say how many one beat may take).
local function default_cost(): number
    return 1
end

--- Whether `v` can be called like a function (a callable table / userdata
--- counts: a Port shim may hand back either).
local function callable(v: any): boolean
    if type(v) == "function" then
        return true
    end
    local mt = getmetatable(v)
    return type(mt) == "table" and mt.__call ~= nil
end

--- A read-only view over `fields`: reads pass through, `pairs` walks the
--- underlying table, and every assignment raises — including one to a key
--- that already exists, which a plain `__newindex` on the table itself
--- would let through.
local function frozen(fields: {string:any}, what: string, tag: string): {string:any}
    -- The metatable is built as data and named as one at the call: `__pairs`
    -- hands back the raw iterator triple and `__metatable` is a protected
    -- tag, and neither is a field of the metatable record Teal declares.
    local mt: {string:any} = {
        __index = fields,
        __newindex = function(_: any, k: any)
            error(what .. " is frozen: '" .. tostring(k) .. "' cannot be assigned", 2)
        end,
        __pairs = function(): (any, any, any)
            return next, fields, nil
        end,
        __len = function(): integer
            -- The underlying table is a map, so this is the `#` of one: the
            -- same 0 the operator answers on it without the metamethod.
            return #(fields as {any})
        end,
        __metatable = tag,
    }
    return setmetatable({} as {string:any}, mt as metatable<{string:any}>)
end

local function shallow_copy(t: {string:any}): {string:any}
    local out: {string:any} = {}
    for k, v in pairs(t) do
        out[k] = v
    end
    return out
end

local function copy_array<V>(t: {V}): {V}
    local out: {V} = {}
    for i, v in ipairs(t or {}) do
        out[i] = v
    end
    return out
end

--- Minimal device config check. Returns an error string, or nil when the
--- config is usable. `llm` is not required here — a device may be built for
--- its tools alone; beat's gate demands one.
---
--- This is the loud half of the pair: `knl.shapes.device_config` says the
--- same thing as data and is asserted beside it, but a dev-mode assert is a
--- no-op in prod and a device built out of a mistyped config would then fail
--- at the first beat instead of at the line that built it. Loud does not
--- mean restated: the entries run against `knl.shapes.tool_entry` here, the
--- same shape `device_config` names. What is left over is the judgement a
--- shape cannot make either way — "callable" (function / `__call`), and "a
--- map of entries, not an array of flat specs", which is a real distinction
--- (`knl_adapter.tools` is what turns the second into the first) and so
--- keeps a sentence of its own.
local function device_problem(config: DeviceConfig): string
    if config.llm ~= nil and not callable(config.llm) then
        return "llm must be a function (or a callable)"
    end
    if config.tools ~= nil then
        if type(config.tools) ~= "table" then
            return "tools must be a map of name -> { description?, input_schema?, handler }"
        end
        -- A map with a first element is an array of flat specs, which is
        -- the mistake this names; the map is read positionally to find out.
        if (config.tools as {any})[1] ~= nil then
            return "tools must be a map (name -> entry); bind an array of specs with knl_adapter.tools first"
        end
        -- Every entry against the shape that declares one, rather than a
        -- hand-written reading of the same triple beside it. The name is in
        -- the message because that is what the caller has to go and fix:
        -- with a dozen tools bound, "handler must be a function" names none
        -- of them.
        for name, entry in pairs(config.tools) do
            local ok, why = shape.check(entry, shapes.tool_entry)
            if not ok then
                return "tools['"
                    .. tostring(name)
                    .. "'] is not { description?, input_schema?, handler }: "
                    .. tostring(why)
            end
        end
    end
    if config.tool_policy ~= nil and type(config.tool_policy) ~= "function" then
        return "tool_policy must be a function"
    end
    if config.fold ~= nil and type(config.fold) ~= "function" then
        return "fold must be a function"
    end
    if config.cost ~= nil and type(config.cost) ~= "function" then
        return "cost must be a function (fn(request) -> integer >= 1)"
    end
    if config.filters ~= nil then
        if type(config.filters) ~= "table" then
            return "filters must be an array of functions"
        end
        for i, filter in ipairs(config.filters) do
            if type(filter) ~= "function" then
                return "filters[" .. i .. "] must be a function"
            end
        end
    end
    return nil
end

local device_with: function(Device, DeviceConfig): Device -- forward declaration (served by the device's __index)

--- Build a device: the resolved, frozen policy half of a beat.
---
--- The config is *consumed* here — defaults are resolved once (`fold` ->
--- `knl.fold`, `filters` -> `{}`, `cost` -> one unit per beat), types are
--- checked once, and the result carries only resolved values. There is no
--- `_config` to read back: what the device does is what its fields say.
--- Unknown keys raise, because a typo in a policy field must not silently
--- become a no-op.
---
--- The device is stateless: share one across sessions, and give one session
--- several (escalation) — nothing about a beat is remembered here.
---
--- @param config table  { llm?, tools?, tool_policy?, fold?, filters?,
---                        system?, cost? }
--- @return table device  frozen, with `with` for derivation
function M.device(config: DeviceConfig): Device
    config = config or {}
    if type(config) ~= "table" then
        error("knl.device: config must be a table", 2)
    end
    -- The accepted key set is the check, so the config is walked as the map
    -- a caller wrote rather than as the record it is declared to be.
    for k in pairs(config as {string:any}) do
        if not IS_DEVICE_KEY[k] then
            error(
                "knl.device: unknown option '"
                    .. tostring(k)
                    .. "' (owner/budget/store/session are state — open or resume a session with them)",
                2
            )
        end
    end
    -- Loud in prod (a construction error must not wait for the first beat)
    -- and shaped in dev (the same contract as data, which is what
    -- `knl.shapes.device_config` publishes).
    local problem = device_problem(config)
    if problem then
        error("knl.device: " .. problem, 2)
    end
    shape.assert_dev(config, shapes.device_config, "knl.device config")

    -- The resolved fields, as the map the device's `__index` reads them out
    -- of: what a device carries is decided here and read back by name.
    local fields: {string:any} = {
        llm = config.llm,
        tool_policy = config.tool_policy,
        system = config.system,
        fold = config.fold or M.fold,
        -- The default answers the same number for every request, so it is
        -- named as the one-argument policy the field declares.
        cost = config.cost or (default_cost as function(any): number),
        -- The caller's arrays/maps are copied, so writing to them after
        -- construction cannot reach the device.
        filters = copy_array(config.filters),
        tools = config.tools and frozen(shallow_copy(config.tools as {string:any}), "a device's tools map", TOOLS_TAG) or nil,
    }

    -- The same arrangement as `frozen`, and for the same two reasons: the
    -- raw iterator triple out of `__pairs`, and a protected `__metatable`
    -- tag `beat` recognises a device by.
    local mt: {string:any} = {
        __index = function(_: any, k: any): any
            if k == "with" then
                return device_with
            end
            return fields[k as string]
        end,
        __newindex = function(_: any, k: any)
            error("a device is frozen: derive a new one with d:with{ " .. tostring(k) .. " = ... }", 2)
        end,
        __pairs = function(): (any, any, any)
            return next, fields, nil
        end,
        __metatable = DEVICE_TAG,
    }
    return setmetatable({} as Device, mt as metatable<Device>)
end

--- Derive a new device: this one's resolved fields with `delta` over them,
--- re-resolved through `knl.device`. The original is untouched and stays
--- usable — a device is a value, and `with` is how a beat gets a different
--- one (`knl.beat(s, d:with{ llm = strong })`).
---
--- @param d table  a knl device
--- @param delta table  device fields to override
--- @return table device'  a new frozen device
device_with = function(d: Device, delta: DeviceConfig): Device
    if type(delta) ~= "table" then
        error("device:with: delta must be a table", 2)
    end
    -- The fields are moved by name, off a value whose own reads go through
    -- the device's `__index`, so both sides are the map they are read as.
    local merged: {string:any} = {}
    for _, k in ipairs(DEVICE_KEYS) do
        merged[k] = (d as {string:any})[k]
    end
    for k, v in pairs(delta as {string:any}) do
        if not IS_DEVICE_KEY[k] then
            error(
                "device:with: '"
                    .. tostring(k)
                    .. "' is not a device field (owner/store/session name a session — open one instead)",
                2
            )
        end
        merged[k] = v
    end
    return M.device(merged as DeviceConfig)
end

-- ============================================================
-- open / resume / session — the state half
-- ============================================================

local OPEN_STATE_KEYS: {string:boolean} = { owner = true, budget = true, store = true, meta = true, parent = true }
local RESUME_STATE_KEYS: {string:boolean} = { store = true, session = true, budget = true }

--- Reject anything that is not a state key. Policy has its own constructor
--- now, so `knl.open{ llm = ... }` is a typo, not a shorthand.
local function state_only(opts: any, allowed: {string:boolean}, who: string)
    if type(opts) ~= "table" then
        error(who .. ": opts must be a table", 3)
    end
    -- The accepted key set is the whole of the check, so the opts are walked
    -- as the map a caller wrote.
    for k in pairs(opts as {string:any}) do
        if not allowed[k] then
            local hint = IS_DEVICE_KEY[k] and " (policy belongs to knl.device)" or ""
            error(who .. ": unknown option '" .. tostring(k) .. "'" .. hint, 3)
        end
    end
end

--- Open a session. The kernel's userdata comes back as-is — this module
--- wraps nothing: `s:append`, `s:events`, `s:reserve`, `s:view`, `s:close`
--- and `<close>` are the kernel's own surface.
---
--- With `parent` it opens a CHILD: on the parent's database, out of the
--- parent's balance (`budget = { from_parent = n, tag? }`), and in one write
--- — the child's opening and grant, and the parent's reservation naming it.
--- A balance that will not cover it records a refusal on the parent and
--- raises `refused`; nothing is opened. Nothing comes back when the child
--- closes, because an allocation is a spend.
---
--- No `store` is the host's database (the header's "Where the log lives");
--- a child needs none, because it goes where its parent already is — and a
--- parent on `"mem"` is refused, since a tree needs a file store.
---
--- `meta` labels the opening, and it is what tells sessions apart in a
--- database that holds many of them: `knl.views.sessions` reads the labels
--- back, and a `session:query` selects on one. Shallow scalars, the same
--- vocabulary `s:append{ meta = … }` takes.
---
--- @param opts table  { owner?, budget? = { amount, tag?, desc? } | { from_parent, tag? }, store?, meta?, parent? }
--- @return userdata session
function M.open(opts: OpenOpts): Session
    opts = opts or {}
    state_only(opts, OPEN_STATE_KEYS, "knl.open")
    shape.assert_dev(opts, shapes.open_opts, "knl.open opts")
    -- The kernel's own userdata, handed back verbatim: the syscall answers
    -- `any` and this is the one place its type is named.
    return bridge("open")({
        owner = opts.owner,
        budget = opts.budget,
        store = opts.store,
        meta = opts.meta,
        parent = opts.parent,
    }) as Session
end

--- Resume a persisted session. The state comes back from the store (the
--- bridge reopens the recorded stream and re-folds the log); policy does NOT
--- — it is a non-serializable closure bundle, so every process builds its
--- own device.
---
--- `store` means what it means on open, so a session opened without one is
--- resumed by its id alone: `knl.resume{ session = id }`.
---
--- @param opts table  { session = <id>, store? = "mem" | { sqlite = <path> }, budget? }
--- @return userdata session  pre-loaded with the log
function M.resume(opts: ResumeOpts): Session
    opts = opts or {}
    state_only(opts, RESUME_STATE_KEYS, "knl.resume")
    shape.assert_dev(opts, shapes.resume_opts, "knl.resume opts")
    return bridge("resume")({
        store = opts.store,
        session = opts.session,
        budget = opts.budget,
    }) as Session
end

--- The canonical bracket: open (or resume),
--- run the body with the session, close it either way.
---
---     local out = knl.session({ owner = "u" }, function(s)
---         return knl.beat(s, d)
---     end)
---
--- Opts naming a `session` resume that stream instead of opening a new one.
--- The body's return values are the bracket's. When the body raises, the
--- session is closed with reason "error" first and the body's error is then
--- re-raised unchanged: a bookkeeping failure must not replace the failure
--- it is bookkeeping for, so a close that itself fails on that path
--- is warned about rather than raised. On the clean path a failing close
--- raises instead, since a bracket
--- that reports success with no boundary recorded is the one outcome this
--- exists to rule out.
---
--- When both fail, the body error wins and the close failure is the
--- suppressed one (try-with-resources' suppressed exception). It is
--- not silent either: it goes to the host `log` global as a warning when
--- the VM has one, as a record — `{ event =
--- "close_failed_after_body_error", body = <the winner, as text>, close =
--- <the kernel's reading, knl.shapes.error> }` — so the loser is at least
--- structured. It cannot be raised (that would replace the body's error)
--- and it cannot be returned (this path does not return), which is why a
--- log is the only place left for it.
---
--- The reason vocabulary is the kernel's, and a normal exit has one word in
--- it: "scope_exit" — what this bracket closes with and what `<close>`
--- records, because leaving the scope is the same event whichever form
--- wrote it. "error" is the failing path (here and in `<close>`), "dropped"
--- the Drop backstop, and "closed" stays the bridge's DEFAULT_CLOSE_REASON
--- for a bare `s:close()`. The message of a body error does not ride along
--- — `s:close` takes a reason and nothing else — so it stays with the
--- error that is propagating.
---
--- @param opts table  knl.open / knl.resume opts
--- @param fn function  fn(session) -> ...
--- @return ...  whatever `fn` returned
function M.session(opts: any, fn: function(Session): any...): any...
    if type(fn) ~= "function" then
        error("knl.session: the second argument must be a function fn(session)", 2)
    end
    opts = opts or {}
    local s: Session
    -- One argument, two constructors: the opts are whichever of the two
    -- shapes they name, and `session` is what tells them apart.
    if (opts as {string:any}).session ~= nil then
        s = M.resume(opts as ResumeOpts)
    else
        s = M.open(opts as OpenOpts)
    end

    local returned = table.pack(pcall(fn, s))
    if returned[1] then
        s:close("scope_exit")
        return table.unpack(returned, 2, returned.n as integer)
    end
    -- The body is failing: close best-effort with the body's error as the
    -- boundary's `detail`, then let that error through. A close that fails
    -- here is reported, not raised and not swallowed — the body's error is
    -- the one that propagates (the suppressed exception of
    -- try-with-resources).
    -- `close` answers nothing, and a `pcall` of it still answers the raise
    -- on the failing path; the handle's method is named with that second
    -- value so the reading below has something to read.
    local closed_ok, cerr = pcall((s.close as function(Session, string, string): any), s, "error", tostring(returned[2]))
    if not closed_ok then
        -- The host's log, if this VM has one: a global read by name, and
        -- whether it answers `warn` is the test right below.
        local host_log = rawget(_G as {string:any}, "log") as {string:any}
        if type(host_log) == "table" and type(host_log.warn) == "function" then
            -- The suppressed failure, as a record rather than a sentence:
            -- which of the two errors this is, the body error that beat it,
            -- and the kernel's reading of the close itself (so a reader can
            -- tell a store that was busy from one that is gone).
            local warning: {string:any} = {
                event = "close_failed_after_body_error",
                body = tostring(returned[2]),
                close = read_error(cerr),
            }
            -- The host's `log.warn` takes a string (see the log bridge's
            -- `msg: String`), so the structured form is offered first and
            -- the text is the fallback. Both are pcall'd: a warning about a
            -- failure must not become one.
            local warned = pcall((host_log.warn as function(any)), warning)
            if not warned then
                pcall(
                    (host_log.warn as function(any)),
                    "knl.session: "
                        .. (warning.event as string)
                        .. ": body="
                        .. (warning.body as string)
                        .. "; close="
                        .. tostring((warning.close as Error).message)
                )
            end
        end
    end
    error(returned[2], 0)
end

--- Mint a beat id: a time-ordered, session-free string the caller stamps on
--- the events of one beat. A module
--- function, not a direct bridge call, so a spec can stand in for it.
---
--- @return string beat_id
function M.new_beat_id(): string
    return bridge("new_beat_id")() as string
end

--- Read a raised kernel failure back as data — the BRIDGE's reader, reached
--- through this module so `require("knl")` is the one surface a script needs.
---
--- Two tables answer to the name `knl`: the syscall bridge the host installs
--- as a global, and this module. A script that wrote `local knl =
--- require("knl")` had shadowed the first, and the reader the docs name is on
--- it — so these two are re-exported here rather than left to a caller
--- reaching past its own local for `rawget(_G, "knl")`. Nothing is
--- reimplemented: the lookup is the same lazy `bridge()` one `new_beat_id`
--- uses, so a spec that installs a fake bridge after this module loaded still
--- reaches it, and a VM with no bridge at all says so rather than indexing nil.
---
--- @param e any  the value a pcall'd syscall raised
--- @return table  `knl.shapes.error` — { kind?, method?, retryable, message }
function M.error(e: any): Error
    return bridge("error")(e) as Error
end

--- The bridge's declared surface, as data — the bridge's reader, read back
--- through the module for the same reason `M.error` is (see above).
---
--- @return table  { session, module, errors, schema, types }
function M.api(): {string:any}
    return bridge("api")() as {string:any}
end

-- ============================================================
-- beat — one complete beat (the primitive; there is no loop here)
-- ============================================================

--- The session surface, as names: every method `knl.shapes.session`
--- declares that a caller can reach (`__close` is the metamethod, reached by
--- the language and not by a call).
---
--- DERIVED from that registry rather than retyped beside it. The list used to
--- be written out here and had drifted — `view` and `query` were syscalls the
--- registry declared and this list did not name, so a stand-in that answered
--- neither passed the gate and failed later, on a call a caller had every
--- right to make. A copy of a list is a list that goes stale; reading the
--- registry is what makes that impossible rather than caught.
---
--- The registry itself is held against the bridge's own `knl.api().session`
--- in both directions where a bridge exists (`tests/fixtures/knl_beat_test.lua`,
--- inv10), so the chain from this gate to the Rust `SESSION_API` is closed.
local SESSION_METHODS: {string} = {}
for name in pairs(M.shapes.session) do
    -- `__close` is the language's, not a call: `local s <close> = ...` reaches
    -- it, and a stand-in that answers every callable method is a session
    -- whether or not it also carries the metamethod.
    if name ~= "__close" then
        SESSION_METHODS[#SESSION_METHODS + 1] = name
    end
end
table.sort(SESSION_METHODS)

--- Whether `s` is a session handle.
---
--- Duck-typing is not a preference here, it is the only test available: the
--- real handle is Rust userdata whose metatable mlua protects, so
--- `getmetatable` answers a boolean rather than the table, and there is no
--- name to compare against from Lua. What is left is to ask the value what
--- it can do.
---
--- So it is asked for the WHOLE declared surface, not the three methods a
--- beat happens to call. A stand-in that answers `append` / `reserve` /
--- `events` and nothing else is not a session — it is a table that would
--- pass the gate and then fail somewhere further in, on a `remaining()` or
--- a `close()` a caller had every right to make. Widening the check is what
--- keeps a spec's fake honest to the surface it stands in for.
local function is_session(s: any): boolean
    local t = type(s)
    if t ~= "table" and t ~= "userdata" then
        return false
    end
    for _, method in ipairs(SESSION_METHODS) do
        -- The reach is by a name held in a variable and the value may be
        -- userdata that raises on it, which is what the pcall is for.
        local ok, value = pcall(function(): any
            return (s as {string:any})[method]
        end)
        if not ok or not callable(value) then
            return false
        end
    end
    return true
end

--- Whether `s` is a session handle — the same judgement `beat` makes, for the
--- callers that have to make it too.
---
--- A policy that takes a session and a supervisor that opens one both have to
--- answer this before they touch the handle, and neither can do it from the
--- outside: the surface is `M.shapes.session`, which is generated from the
--- Rust side's own types, and the real handle is userdata whose metatable is
--- protected. Every consumer that asked it for itself asked a smaller
--- question — one method instead of the surface, `type(s) == "table"` instead
--- of both types — and one of those refused a perfectly good session in a
--- run. Published, there is one answer and it moves with the declaration.
---
--- @param s any
--- @return boolean
function M.is_session(s: any): boolean
    return is_session(s)
end

--- Append an event this beat is writing, through the dev-mode contract.
---
--- Named `record_event` rather than `record`: `record` is how Teal spells a
--- type declaration, and a local cannot take that name.
local function record_event(session: Session, ev: {string:any}): kt.Seq
    -- The envelope the kernel takes is the one this gate just held the
    -- event to, so it is named as that on the way through.
    return session:append(assert_event_dev(ev) as kt.AppendEvent)
end

--- Stamp the beat id on an event about to be written.
---
--- The id lives in `meta` — it is a label, `meta.beat`, and not a field of
--- its own — so stamping it is a merge and not an assignment: an event that
--- already carries labels keeps them and `beat` joins them. The kernel's id
--- wins over one already under that key, because the kernel is what mints
--- it. `meta` values stay scalars either way; the id is a string.
---
--- `ev` is written into rather than copied: every call site is a literal
--- built on the spot for the append that follows.
---
--- @param ev table  the event this beat is writing
--- @param beat_id string  the id of the beat writing it
--- @return table ev  the same event, with `meta.beat` set
local function with_beat(ev: {string:any}, beat_id: string): {string:any}
    local meta: {string:any} = {}
    for k, v in pairs((ev.meta or {}) as {string:any}) do
        meta[k] = v
    end
    meta.beat = beat_id
    ev.meta = meta
    return ev
end

--- What is wrong with an llm's answer, or nil when nothing is.
---
--- `device.llm` promises one of two things: an `llm_result` (`knl.shapes`),
--- or `nil` and an error. The result has exactly two statuses, a `usage` the
--- adapter has already normalized into three counts, and — on a refusal —
--- the `refusal.kind` that says what refused. An answer that keeps none of
--- that is a broken adapter, and beat ends the beat the way any other failed
--- call ends rather than filling the gaps in: a defaulted usage would put a
--- count nobody reported into the history, and a refusal reported as
--- "refused" would be beat naming a reason it was never given.
---
--- @param resp any  whatever `device.llm` answered
--- @return string|nil  the violation, or nil when the answer is well formed
local function llm_contract_violation(resp: any): string
    if type(resp) ~= "table" then
        if resp == nil then
            return "llm answered neither a result nor an error"
        end
        return "llm returned " .. type(resp) .. " (the contract is a table, or nil and an error)"
    end
    -- Past the type test the value is held to `llm_result`, which is the
    -- whole of what this function is asking about.
    local answer = resp as LlmResult
    if answer.status ~= "ok" and answer.status ~= "refused" then
        return "llm answered status " .. tostring(answer.status) .. ' (llm_result is "ok" or "refused")'
    end
    if type(answer.usage) ~= "table" then
        return "llm answered a " .. type(answer.usage) .. " usage (llm_result promises the three counts)"
    end
    if answer.status == "refused" and type(answer.refusal) ~= "table" then
        return "llm refused without a refusal (llm_result promises refusal.kind on a refusal)"
    end
    if answer.status == "refused" and type((answer.refusal as {string:any}).kind) ~= "string" then
        return "llm refused without a refusal.kind (llm_result promises the class that refused)"
    end
    return nil
end

--- Ask the policy about one tool_use block.
---
--- The contract is `tool_policy(tool_use_block, out) -> decision, reason?`
--- with a decision of `nil` (no opinion — run), `"run"` or `"deny"`, and
--- nothing else: a fourth word is a device-contract violation rather than a
--- fourth meaning this layer gets to guess at. A policy that RAISES is
--- fail-closed — a gate written to veto tools must not fall open on its own
--- bug — so the raise denies the call and its message becomes the reason.
---
--- @param policy function|nil  device.tool_policy
--- @param block table  the tool_use block being decided
--- @param out table  the model response it came in
--- @return string|nil action  "run" / "deny", or nil on a violation
--- @return string|nil reason  the denial's reason, when there is one
--- @return string|nil problem  the contract violation, when there is one
local function decide_tool(policy: function(any, any, any): any, block: {string:any}, out: any): string, string, string
    if policy == nil then
        return "run"
    end
    -- The policy is the caller's own function and is called with the two
    -- arguments its contract names; `pcall` takes it as the plain callable
    -- it is.
    local ok, decided, reason = pcall((policy as function(...: any): any...), block, out)
    if not ok then
        return "deny", "policy raised: " .. tostring(decided)
    end
    -- The published shape is the vocabulary, checked (not asserted) so the
    -- judgement is the same in prod as in dev: a decision outside it is a
    -- contract violation to report, not a raise.
    if not shape.check(decided, shapes.tool_policy_decision) then
        return nil,
        nil,
        "tool_policy returned "
            .. (type(decided) == "string" and string.format("%q", decided as string) or type(decided))
            .. ' (a decision must be nil, "run" or "deny")'
    end
    -- Past the check there are three values left: nil (no opinion), "run",
    -- and "deny".
    if decided ~= "deny" then
        return "run"
    end
    -- A reason is optional and a string; anything else is not made up into
    -- one, it is left out.
    return "deny", ((type(reason) == "string" and reason ~= "" and reason) or nil) as string
end

--- Run the tool_use blocks of a response, closing every one with a
--- tool_result. What runs / is denied is `device.tool_policy`, the success
--- result is the handler's, and the pair-closing record — including the
--- machine-minimal error for an unknown tool, a denied call or a raising
--- handler — is the kernel's. `beat_id` stamps both halves of every pair,
--- so the pair reads back as part of its beat.
---
--- Every block is decided before any of them runs. A policy that breaks its
--- contract therefore stops the beat with nothing dispatched and no
--- tool_call written, rather than half a response's worth of side effects
--- and a report that the config was wrong.
---
--- The handler and the policy are the caller's code, like fold and the llm,
--- but their raises are not traced: a raising handler or policy is closed
--- as DATA — the `result` of a tool_result, a durable record a later fold
--- reads back and sends to the model — and a record that carried a stack in
--- dev and not in prod would be two different histories. The traceback is
--- attached where a failure is *reported* (an Outcome the caller reads and
--- drops), never where one is *recorded*.
---
--- @param session userdata|table  a knl session
--- @param device table  a knl device
--- @param out table  the model response being executed
--- @param beat_id string  the id of the beat writing these events
--- @return table|nil summary  one { call_id, name, ok } per tool_use
--- @return string|nil problem  a device-contract violation (nothing ran)
local function execute_tools(session: Session, device: Device, out: any, beat_id: string): {any}, string
    local tools = device.tools or {}
    local policy = device.tool_policy

    -- [a] decide everything first (nothing has run, nothing is recorded)
    local planned: {{string:any}} = {}
    -- The response's content is the array of blocks the header's
    -- *provider-neutral* shape names, read by key like every other one.
    for _, block in ipairs(((out as {string:any}).content or {}) as {{string:any}}) do
        if block.type == "tool_use" then
            local action, reason, problem = decide_tool(policy, block, out)
            if problem then
                return nil, problem
            end
            planned[#planned + 1] = { block = block, action = action, reason = reason }
        end
    end

    -- [b] then run them, each pair recorded around its call
    local summary: {any} = {}
    for _, item in ipairs(planned) do
        local block = item.block as {string:any}
        -- Read, not repaired. A `tool_use` block names its call id and its
        -- tool (`knl.shapes.tool_use_block`); a block that named neither is
        -- the llm breaking that contract, and standing an empty string in
        -- for the missing name would turn it into a tool_result about a
        -- tool called "" — a fabricated fact in a durable record.
        local call_id = block.id
        -- The name is the llm's own and is taken as it came: a block that
        -- named none fails at the concatenation below, which is the
        -- contract breaking rather than a hole this layer fills.
        local name = block.name as string
        local args = block.input or {}

        -- Record the call before running it: a run that dies mid-tool
        -- leaves a history that says a call was made.
        record_event(
            session,
            with_beat({
                kind = "tool_call",
                data = {
                    call_id = call_id,
                    name = name,
                    args = args,
                },
            }, beat_id)
        )

        local ok, result: boolean, any
        if item.action == "deny" then
            local why = item.reason and (": " .. (item.reason as string)) or ""
            ok, result = false, "tool '" .. name .. "' denied by policy" .. why
        else
            local spec = tools[name]
            if spec == nil or spec.handler == nil then
                -- Unknown tool: close the pair with a machine-minimal
                -- error. This is the kernel's skeleton, not a policy.
                ok, result = false, "tool '" .. name .. "' not found"
            else
                local pok, pres = pcall(spec.handler, args)
                if pok then
                    ok, result = true, pres
                else
                    ok, result = false, "tool '" .. name .. "' raised: " .. tostring(pres)
                end
            end
        end

        -- The empty result, defined: a tool_result carries a `result`, and a
        -- handler that answered nothing answered the empty string. This is
        -- the rule, not a stand-in for a missing value — "the tool ran and
        -- produced no output" is a real outcome, and it is what a later fold
        -- sends back to the model.
        if result == nil then
            result = ""
        end

        record_event(
            session,
            with_beat({
                kind = "tool_result",
                data = {
                    call_id = call_id,
                    ok = ok,
                    result = result,
                },
            }, beat_id)
        )

        summary[#summary + 1] = { call_id = call_id, name = name, ok = ok }
    end

    return summary
end

--- One complete beat: gate, name itself, fold, filter, reserve, record,
--- call, record, run its tools. Two arguments and no bundle — the session
--- is the kernel's state, the device is the caller's policy, and neither is
--- mutated. Per-beat variation is the caller's: `knl.beat(s, d:with{ llm =
--- strong })`.
---
--- Re-entrant and stateless: a beat is decided entirely by its two
--- arguments, so it can be called from any driver, resumed, or interleaved.
---
--- What the fold left out is recorded, not decided on
---   A fold may answer a second value beside the request — a report of what
---   it dropped to build it (`knl.shapes.window_report`: the beat ids, how
---   many stayed, whether the seed did, and the token counts when it counted
---   any). beat writes it onto the `llm_request` event as `data.window` and
---   reads it nowhere: a windowed request is indistinguishable from a short
---   conversation once it is in the log, and this is the fact that tells them
---   apart afterwards. Nothing branches on it, no policy is applied to it, and
---   a fold that answers one value (the default one does) leaves it absent.
---
--- Every syscall it makes is pcall'd and every failure comes back as an
--- Outcome — `err("state")`, carrying the kernel's own reading of the raise
--- (`detail.kind` / `detail.retryable`, `knl.shapes.error`). beat does NOT
--- act on `retryable`: a beat that quietly repeated a `busy` reserve would
--- be a loop nobody wrote and nobody bounded, and it would make a second
--- attempt at a call the caller may no longer want. Asking again is the
--- caller's loop's decision, and this is the value it decides from.
---
--- @param session userdata|table  a knl session (knl.open / knl.resume)
--- @param device table  a knl device (knl.device / d:with)
--- @return table outcome  an `Outcome`
function M.beat(session: Session, device: Device): OutcomeValue
    -- [0] gate ------------------------------------------------------------
    if not is_session(session) then
        return emit(Outcome.err("conf", "beat takes a knl session first (from knl.open / knl.resume)"))
    end
    -- A device's `__metatable` is the protected string tag it was built
    -- with, so the comparison is against that value and not a metatable.
    if (getmetatable(device) as any) ~= DEVICE_TAG then
        return emit(Outcome.err("conf", "beat takes a knl device second (build one with knl.device)"))
    end
    -- The llm is beat's to call, off the device.
    if device.llm == nil then
        return emit(Outcome.err("conf", "no llm in the device (knl.device{ llm = ... } / d:with{ llm = ... })"))
    end

    -- [0.5] the beat names itself ----------------------------------------
    -- One id per beat, minted here and stamped on every event this beat
    -- writes. The kernel neither numbers nor requires it.
    local id_ok, beat_id = pcall(M.new_beat_id)
    if not id_ok or type(beat_id) ~= "string" then
        return emit(Outcome.err("conf", "no beat id: " .. tostring(beat_id)))
    end

    -- [1] request <- fold(events, device) ---------------------------------
    -- Reading the log is a kernel call like any other in this function: a
    -- store that cannot be read (a closed session, a failed query) is
    -- `err("state")`, not a raise escaping beat's Outcome contract. It is
    -- pcall'd separately from the fold so the two failures keep their own
    -- kinds — the state that could not be read, or the policy that could
    -- not fold it.
    local read_ok, events, truncated = pcall(session.events, session)
    if not read_ok then
        -- The kernel's own reading of its own failure: `events` holds the
        -- raise on this path, and what a caller needs off it (which class,
        -- whether asking again is worth anything) is in the table, not in
        -- a sentence somebody would have to parse.
        return emit(Outcome.err("state", read_error(events)))
    end
    -- A read that stopped at the kernel's row cap is not this history. The
    -- cap counts FORWARD from `from`, so what a truncated read is missing is
    -- the END — the most recent turns, the ones a request is mostly about —
    -- and folding it would send the model a conversation that stops in the
    -- middle and looks, from the other side, exactly like a conversation that
    -- ended there. So the beat refuses instead, here at the read, which is
    -- the only place the fact is known: `fold` is a pure function of an event
    -- array (a caller's own fold, and `supervisor.merge`, hand it arrays that
    -- never came from this call at all) and has nothing to read the flag off.
    --
    -- Unattributed, like any raise the bridge did not classify: this is not
    -- one of the kernel's error classes, it is the shell declining to build a
    -- request out of a partial record.
    if truncated then
        return emit(Outcome.err("state", {
            retryable = false,
            message = "knl.beat: the history is longer than one read of it, and a fold over a "
                .. "truncated record would leave out the most recent events; page it with "
                .. "session:events(from) or fold a window (policy.window)",
        }))
    end
    -- The fold is the caller's code, so its failures are traced (dev mode).
    --
    -- A fold may answer a SECOND value: a report of what it left out
    -- (`knl.shapes.window_report`). It is carried to [4] and written on the
    -- `llm_request` and read nowhere else — the beat decides nothing on it.
    -- The default fold answers one value, and so does a caller's own unless
    -- it windows; `report` is simply nil then.
    -- A fold is the caller's own function and may answer one value or two,
    -- so it is called as the plain callable it is and both are read.
    local folded_ok, request, report = xpcall((device.fold as function(...: any): any...), traced, events, device)
    if not folded_ok then
        return emit(Outcome.err("conf", raised_detail("fold failed: " .. raised_text(request), request)))
    end

    -- [2] filter chain (fn(req) -> req) -----------------------------------
    for i, filter in ipairs(device.filters) do
        local filtered_ok, filtered = xpcall(filter, traced, request)
        if not filtered_ok then
            return emit(Outcome.err("filter", raised_detail(raised_text(filtered), filtered)))
        end
        -- A filter's return replaces the request wholesale, so a filter
        -- that mutates and forgets to return (nil) or returns a
        -- non-table would corrupt the write-ahead record and the wire.
        -- Loud in prod, not just under the dev assert below.
        if type(filtered) ~= "table" then
            return emit(
                Outcome.err(
                    "filter",
                    "filter #" .. i .. " returned " .. type(filtered) .. " (a filter must return the request table)"
                )
            )
        end
        request = filtered
    end
    -- Dev-mode contract on what actually goes to the llm — a custom fold or
    -- a filter that broke the request shape fails loud here, not in the wire.
    shape.assert_dev(request, shapes.request, "knl_request")

    -- [3] reserve before anything is recorded or called -------------------
    -- The quota decides here, with the request known and nothing spent yet:
    -- a refusal leaves no `llm_request` event, makes no call, and is a planned
    -- stop rather than a failure (`stopped`, carrying the grant's tag so a
    -- caller can name what stopped it). How much to ask for is the device's
    -- policy — `device.cost(request)` — and it is never derived from token
    -- counts: the budget and `knl.views.usage` are separate readings (the
    -- *budget* section of the header).
    local est_ok, amount = xpcall(device.cost, traced, request)
    if not est_ok then
        return emit(Outcome.err("conf", raised_detail("cost failed: " .. raised_text(amount), amount)))
    end
    -- `cost_result` carries the type; the bound and the integrality are
    -- here, because lshape has no combinator for either. Checked rather than
    -- asserted: this must be loud in prod too — an unbounded beat is how a
    -- run stops being finite.
    if not shape.check(amount, shapes.cost_result) or (amount as number) < 1 or (amount as number) % 1 ~= 0 then
        return emit(Outcome.err("conf", "cost must return a whole number >= 1, got " .. tostring(amount)))
    end
    -- The handle raises on a closed session, and beat's contract is an
    -- Outcome, so the call is pcall'd.
    local res_ok, granted, tag = pcall(session.reserve, session, amount as kt.Amount)
    if not res_ok then
        -- `granted` holds the raise here; `busy` is the class a loop may act on.
        return emit(Outcome.err("state", read_error(granted)))
    end
    if not granted then
        return emit(Outcome.stopped("budget", tag as string))
    end

    -- [4] record the request write-ahead (open kind "llm_request") --------
    -- The request as actually sent is a fact in the history before the call,
    -- so a call that then fails leaves the llm_request event behind.  An append
    -- can fail (closed session, an unavailable store, validation) — beat's
    -- contract is an Outcome, so a state failure is Error("state"), never
    -- a raw raise.
    --
    -- What the fold left out to build that request goes on the same event, as
    -- `data.window`, when the fold answered one: the request itself cannot
    -- say it — a windowed conversation just begins later — and the beat that
    -- sent it is the only moment the fact exists. It is written and never
    -- read back here; `knl.views.beats` is what reads it, and a caller may.
    -- Only a table is taken: a fold answering something else beside the
    -- request has not made a report, and a stray second value must not become
    -- a durable one.
    local request_data: {string:any} = { request = request }
    if type(report) == "table" then
        request_data.window = report
    end
    local rec_ok, rec_err = pcall(
        record_event,
        session,
        with_beat({
            kind = "llm_request",
            data = request_data,
        }, beat_id)
    )
    if not rec_ok then
        return emit(Outcome.err("state", read_error(rec_err)))
    end

    -- [5] beat calls the llm directly ------------------------------------
    -- resp = an `llm_result`: { status = "ok"|"refused", content, usage,
    -- stop_reason?, finish_reason?, refusal? }.  The status is the adapter's judgement; beat
    -- reads it, it does not invent one (status is llm-supplied).  The call is
    -- caught: an adapter that raises (instead of returning nil, err) is still
    -- a call failure, not an escape from the Outcome contract — and it is the
    -- caller's own code, so the raise is traced (dev mode).
    local call_ok, resp, berr = xpcall((device.llm as function(...: any): any...), traced, request)
    local raised: any
    if not call_ok then
        raised = resp
        berr = "llm raised: " .. raised_text(raised)
        resp = nil
    end

    -- [6] the answer, held to its contract --------------------------------
    -- One ending for every way the call did not come off: a transport or
    -- provider failure (`nil, err` — its own message is the reason), a raise,
    -- or an answer that is not an `llm_result`. Note it and stop.  The
    -- failure note is best-effort (the state may be what failed); the call
    -- error stays the primary detail either way.
    --
    -- The contract is judged rather than worked around. There is no third
    -- status to read (`llm_result` has two, and a failure is `nil, err`), no
    -- usage to default and no refusal reason to invent: an adapter that broke
    -- its promise has produced no response, and this is where that is said.
    local reason: string
    if type(resp) ~= "table" then
        reason = (berr as string) or llm_contract_violation(resp)
    else
        reason = llm_contract_violation(resp)
    end
    if reason ~= nil then
        -- The failure, classified: what the port said if it said anything,
        -- and `unknown` otherwise (`call_error`). One table, so a loop reading
        -- `detail.kind` / `detail.retryable` gets an answer here for the same
        -- reason it gets one from a "state" failure — and so the note in the
        -- log says the same thing the Outcome does.
        local classified = call_error(reason, raised)
        shape.assert_dev(classified, shapes.call_error, "knl_call_error")
        local noted_ok, note_err = pcall(
            record_event,
            session,
            with_beat({
                kind = "llm_call_failed",
                data = {
                    error = classified.message,
                    kind = classified.kind,
                    retryable = classified.retryable,
                    retry_after = classified.retry_after,
                    status = classified.status,
                },
            }, beat_id)
        )
        if not noted_ok then
            -- Two failures, one Outcome. The state is the one reported:
            -- the call failing is a fact this beat could not write down,
            -- and a caller that cannot write cannot go on either way. The
            -- kernel's reading of the append is the detail (so a loop can
            -- still tell contention from a dead store), and the call's own
            -- reason — the note that did not land — rides along as `cause`
            -- (the suppressed failure, not the winner).
            local detail = read_error(note_err)
            detail.cause = classified.message as string
            return emit(Outcome.err("state", detail))
        end
        return emit(Outcome.err("call", classified))
    end
    -- ok / refused: the model answered. Appending the llm_response is what
    -- records it, under this beat's id, and the budget does not move (the
    -- deduction was taken at [3]).  The counts go in as they came: the adapter
    -- normalized them to three numbers on its way out, so there is nothing
    -- here to default and nothing to invent.
    local resp_ok, resp_err = pcall(
        record_event,
        session,
        with_beat({
            kind = "llm_response",
            data = {
                content = (resp as {string:any}).content,
                usage = (resp as {string:any}).usage,
                stop_reason = (resp as {string:any}).stop_reason,
                finish_reason = (resp as {string:any}).finish_reason,
            },
        }, beat_id)
    )
    if not resp_ok then
        return emit(Outcome.err("state", read_error(resp_err)))
    end
    -- The answer is the port's own table and the beat stamps its id on it;
    -- what came back is `any`, so the write is named at the key.
    (resp as {string:any}).beat = beat_id
    -- A refusal is a recorded response the model would not build on — and
    -- the beat that produced it reserved its amount like any other. What
    -- refused is the adapter's classification (`refusal.kind`: the model
    -- itself, or a provider's filter), which is the one place that judgement
    -- is made; `stop_reason` is the provider's own word for the same moment
    -- and beat does not translate one into the other.
    if (resp as LlmResult).status == "refused" then
        return emit(Outcome.refused(((resp as LlmResult).refusal as {string:any}).kind as string, resp))
    end

    -- [7] tool execution (skeleton) --------------------------------------
    -- execute_tools raises only on a state failure (an append that did not
    -- land); handler and policy failures close their pair as data
    -- (ok=false). A tool_policy that broke its contract is the one thing it
    -- reports instead: a config error, returned before any tool ran or any
    -- tool_call was written. The llm_response above is already recorded
    -- either way — the beat happened, it is the device that is wrong.
    local tools_ok, summary, policy_problem = pcall(execute_tools, session, device, resp, beat_id)
    if not tools_ok then
        -- Only an append raises out of execute_tools (a handler's or a
        -- policy's raise is closed as data inside it), so this is the
        -- kernel's failure and is read like any other.
        return emit(Outcome.err("state", read_error(summary)))
    end
    if policy_problem then
        return emit(Outcome.err("conf", policy_problem))
    end
    (resp as {string:any}).tools = summary

    return emit(Outcome.ok(resp))
end

-- An internal exposed for the spec, which drives it directly.
M._execute_tools = execute_tools

-- ============================================================
-- views — the query views this module ships
-- ============================================================
--
-- A view is a named function that runs one SELECT. That is the whole of the
-- mechanism: no builder, no query object, no registration hook. The kernel
-- publishes the table (`knl.shapes.schema`) and a caller writes SQL against
-- it, and these four are the ones the kernel ships because they read what
-- the kernel itself wrote — the beat grouping it stamps, the tool pairs it
-- closes, the ledger it keeps, the token counts the providers reported. A
-- consumer's own view is a function of exactly this form in exactly this way
-- (`local function tool_error_rate(s) return s:query([[...]]) end`); nothing
-- here is privileged.
--
-- They do not duplicate the built-in reads, which are `events(from)` and
-- `tail(n)` and nothing else. Token usage is NOT one of them: it is an
-- aggregate over the log like any other question a caller asks of it, so it
-- is `knl.views.usage` — one SELECT, in this file, on the same footing as a
-- view a consumer writes.
--
-- Two rules hold for every statement below and for a consumer's own:
--
--   * the streams being read are named by `$sessions`, never spliced in.
--     The kernel expands the token into one bound placeholder per id, so an
--     id is a value like any other and `opts.sessions` reads a set of
--     sessions with the same SQL that reads one;
--   * no value is concatenated into the text. What is written into these
--     statements is column names, the kernel's own kind vocabulary, and
--     nothing that came from a caller.
--
-- Which half of the stored event a statement reads decides what can break
-- it. `beat` is a `meta` key — `meta` is the envelope's correlation bag, and
-- the beat id is a label in it — so `beats` groups on
-- `json_extract(meta, '$.beat')` and is untouched by any change to what a
-- kind carries. That the reach is a JSON path does not change the argument:
-- what it reaches into is the envelope, whose keys are labels this layer
-- stamps, and not some kind's `data`. The rest do reach into `data`, and
-- each of those paths is tied to one kind's shape: `tool_pairs` to the tool pair,
-- `ledger` to `budget_*`, `usage` to `llm_response`. A kind's shape and the
-- view that reads it change together, which is the whole reason the
-- structured half lives in one column instead of being spread over the row.

--- One row per beat: where it starts, where it ends, and what it wrote.
---
--- `kinds` is the beat's events in `seq` order, comma-joined. The order
--- comes from the ordered subquery rather than from `group_concat(kind ORDER
--- BY seq)`: the aggregate's own ORDER BY needs SQLite 3.44, and SQLite may
--- not flatten a subquery with an ORDER BY into an aggregating outer query,
--- so the rows reach the aggregate in the order the subquery put them.
---
--- Events with no `meta.beat` — the session's own boundaries, the ledger, a
--- caller's seed message — are not part of any beat and are left out. The
--- grouping key is a `meta` label, so the grouping itself reads nothing out
--- of any kind's `data` and no change to one can reach it.
---
--- Three columns do reach into `data`, and into exactly one kind's:
--- `dropped` / `before` / `after` come off the beat's own `llm_request`,
--- where `knl.beat` records what the fold left out (`data.window`,
--- `knl.shapes.window_report`). `dropped` is the LENGTH of the id list
--- rather than the ids — a row is one beat, and the ids of the beats it
--- dropped are a list inside it that a caller reads off the event when it
--- wants them.
---
--- The `CASE WHEN kind = 'llm_request'` is what keeps that tie to one kind:
--- without it the paths would be selected out of every event of the beat, and
--- a future kind carrying a `window` key would silently join in. So this view
--- moves with the shape of `llm_request` and with nothing else. A beat whose
--- request carried no report answers NULL — `json_extract` of a missing path
--- is NULL, and `json_array_length` of NULL is NULL — which is the honest
--- answer for a fold that windowed nothing, and not a zero it never reported.
local BEATS_SQL = [[
SELECT beat,
       MIN(seq)           AS seq_from,
       MAX(seq)           AS seq_to,
       group_concat(kind) AS kinds,
       MAX(dropped)       AS dropped,
       MAX(counted_before) AS "before",
       MAX(counted_after)  AS "after"
  FROM (SELECT json_extract(meta, '$.beat') AS beat,
               stream,
               seq,
               kind,
               CASE WHEN kind = 'llm_request'
                    THEN json_array_length(json_extract(data, '$.window.dropped'))
               END AS dropped,
               CASE WHEN kind = 'llm_request'
                    THEN json_extract(data, '$.window.before')
               END AS counted_before,
               CASE WHEN kind = 'llm_request'
                    THEN json_extract(data, '$.window.after')
               END AS counted_after
          FROM events
         WHERE stream IN $sessions
           AND json_extract(meta, '$.beat') IS NOT NULL
         ORDER BY stream, seq)
 GROUP BY beat
 ORDER BY seq_from, beat
]]

--- The tool pairs: a `tool_call` and the `tool_result` that answered it,
--- joined on the call id within one stream.
---
--- A call id is unique to the stream that minted it, so the join carries
--- `r.stream = c.stream` — without it a set of sessions read together could
--- pair one session's call with another's result.
---
--- A call with no result is not a pair and does not appear. That is the
--- point of the view: what it lists is the calls that were answered, and a
--- call left open by a run that died mid-tool is visible as its absence
--- (`beats` still shows the `tool_call` in its `kinds`).
--- The `beat` comes off the envelope's labels and the rest out of `data`:
--- this view is tied to the shape of `tool_call` / `tool_result`
--- (`knl.shapes.events`) and moves with it.
local TOOL_PAIRS_SQL = [[
SELECT json_extract(c.meta, '$.beat')    AS beat,
       json_extract(c.data, '$.call_id') AS call_id,
       json_extract(c.data, '$.name')    AS name,
       json_extract(r.data, '$.ok')      AS ok
  FROM events AS c
  JOIN events AS r
    ON r.stream = c.stream
   AND r.kind = 'tool_result'
   AND json_extract(r.data, '$.call_id') = json_extract(c.data, '$.call_id')
 WHERE c.stream IN $sessions
   AND c.kind = 'tool_call'
 ORDER BY c.stream, c.seq
]]

--- The budget ledger: every `budget_*` event in order, with the amount and
--- the grant's tag read out of `data`. Those are kernel kinds, so their
--- `data` shape is the kernel's — this is the one view whose paths belong to
--- the other side of the syscall.
---
--- The four kinds are named rather than matched with a `LIKE 'budget_%'`:
--- they are the closed vocabulary the balance is a fold of (event.rs), and
--- `kind` is an indexed column, so naming them keeps the read the size of
--- the ledger rather than the size of the stream.
local LEDGER_SQL = [[
SELECT seq,
       kind,
       json_extract(data, '$.amount') AS amount,
       json_extract(data, '$.tag')    AS tag
  FROM events
 WHERE stream IN $sessions
   AND kind IN ('budget_granted', 'budget_reserved', 'budget_refused', 'budget_spent')
 ORDER BY stream, seq
]]

--- The token accounting: one row per stream, over the `llm_response` events
--- that stream recorded.
---
--- `calls` is how many answers landed and the three counters are what the
--- providers reported for them — facts already in the log, since an adapter
--- normalizes a provider's usage into the three numbers before the response
--- is ever appended. Nothing here is a budget: the grant is a quota the owner
--- gave (`ledger` is its reading), and no arithmetic connects the two.
---
--- A counter a stored response does not carry contributes 0: `json_extract`
--- answers NULL for a missing key, `SUM` skips it, and the `COALESCE` turns
--- an all-NULL sum back into a number so a caller never reads a nil count.
---
--- The grouping is by `stream`, which is what makes the row set answer for a
--- read across a set of sessions rather than blending them. A stream that
--- recorded no response has no row at all — that absence IS its zero, and
--- filling it in would mean naming the streams in the statement, which is
--- exactly what `$sessions` exists not to do.
local USAGE_SQL = [[
SELECT stream,
       COUNT(*)                                                         AS calls,
       COALESCE(SUM(json_extract(data, '$.usage.input_tokens')), 0)     AS input_tokens,
       COALESCE(SUM(json_extract(data, '$.usage.output_tokens')), 0)    AS output_tokens,
       COALESCE(SUM(json_extract(data, '$.usage.thinking_tokens')), 0)  AS thinking_tokens
  FROM events
 WHERE stream IN $sessions
   AND kind = 'llm_response'
 GROUP BY stream
 ORDER BY stream
]]

--- The session tree, rooted at the session the view is called on.
---
--- The one view that does not take its streams from `$sessions`, and the
--- reason is what a tree is: the set is not something a caller names, it is
--- what the log says was opened from what. So the root is `$stream` — this
--- session — and the walk follows `session_opened.data.parent` down from it
--- with a recursive CTE, which is a `WITH` statement and therefore a read
--- like any other (the query layer needed no change for this: a statement
--- sees the `events` table, and `$stream` / `$sessions` are values bound
--- into it rather than a fence around what it may look at).
---
--- `UNION` and not `UNION ALL`: a stream is in the subtree once, and the
--- duplicate-eliminating form is also what stops a `parent` cycle — which
--- the kernel does not prevent and a log written by hand could contain —
--- from running forever.
---
--- The three per-session readings are correlated subqueries rather than
--- joins, so a stream that recorded two endings (two handles that both
--- closed — the log keeps both) is still one row: the first ending is the
--- one reported, and `MIN(epoch_ms)` says the same for the opening.
---
--- `open_children` comes back as the JSON array text the close recorded,
--- because that is what the column holds: `json_extract` of an array is its
--- JSON, and re-encoding it into a Lua list here would be this view
--- inventing a shape the log does not have.
local TREE_SQL = [[
WITH RECURSIVE tree(session, parent) AS (
    SELECT root.stream, json_extract(root.data, '$.parent')
      FROM events AS root
     WHERE root.kind = 'session_opened'
       AND root.stream = $stream
    UNION
    SELECT child.stream, json_extract(child.data, '$.parent')
      FROM events AS child, tree
     WHERE child.kind = 'session_opened'
       AND json_extract(child.data, '$.parent') = tree.session
)
SELECT t.session AS session,
       t.parent  AS parent,
       (SELECT MIN(o.epoch_ms) FROM events AS o
         WHERE o.stream = t.session AND o.kind = 'session_opened') AS opened_epoch_ms,
       (SELECT MIN(c.epoch_ms) FROM events AS c
         WHERE c.stream = t.session AND c.kind = 'session_closed') AS closed_epoch_ms,
       (SELECT json_extract(c.data, '$.open_children') FROM events AS c
         WHERE c.stream = t.session AND c.kind = 'session_closed'
         ORDER BY c.seq LIMIT 1) AS open_children
  FROM tree AS t
 ORDER BY opened_epoch_ms, session
]]

--- Every session the log holds, with the labels it was opened under.
---
--- The companion of `TREE_SQL`, and the other question a supervisor asks of
--- a database that holds many runs: `tree` walks the sessions one opening
--- descends from, this one lists them all. A caller that gave its runs a
--- label (`knl.open{ meta = { run = id } }`) reads them back here without
--- having given each one a database.
---
--- `meta` is the column's own JSON text, for the reason `open_children` is:
--- the log holds it as written, and a view that re-encoded it into a Lua
--- table would be inventing a shape the log does not have. The rest is read
--- the same way `tree` reads it — correlated subqueries, so a stream that
--- recorded two endings is still one row.
local SESSIONS_SQL = [[
SELECT o.stream    AS session,
       o.meta      AS meta,
       json_extract(o.data, '$.parent') AS parent,
       MIN(o.epoch_ms) AS opened_epoch_ms,
       (SELECT MIN(c.epoch_ms) FROM events AS c
         WHERE c.stream = o.stream AND c.kind = 'session_closed') AS closed_epoch_ms
  FROM events AS o
 WHERE o.kind = 'session_opened'
 GROUP BY o.stream
 ORDER BY opened_epoch_ms, session
]]

--- Run one view's statement over `session`.
---
--- The options are the caller's, passed through untouched: `sessions` is
--- what makes a view span a set of streams, and `timeout_ms` / `limit` are
--- the same knobs any other read has. No view takes parameters of its own —
--- the only values any of them binds are the stream ids the kernel resolves
--- from `$sessions`.
---
--- `truncated` is handed back beside the rows rather than dropped: a view
--- that had more rows than the limit allowed has said so, and swallowing
--- that would leave a caller to guess from a suspiciously round count.
local function read_view(session: Session, sql: string, opts: QueryOpts): {{string:any}}, boolean
    return session:query(sql, nil, opts)
end

--- Whether a stored `ok` is true.
---
--- SQLite has no boolean: `json_extract` answers 1 / 0 for a JSON true /
--- false, and that is what crosses the bridge. The view declares an `ok`, so
--- the reading back into a boolean happens here — once, in the layer that
--- promised it — rather than in every caller.
local function truthy(value: any): boolean
    return value == true or value == 1
end

M.views = {}

--- One row per beat: `{ beat, seq_from, seq_to, kinds, dropped, before,
--- after }`.
---
--- The first four are the beat's own extent, read off the envelope. The last
--- three are what its fold left out, read off that beat's `llm_request`
--- (`data.window`): how many beats it dropped, and what the request counted
--- before and after the dropping. All three are nil for a beat whose request
--- carried no report — a fold that windows nothing reports nothing — and
--- `before` / `after` are nil as well for a fold that windowed by a count of
--- beats and never counted tokens.
---
--- @param session userdata|table  a knl session
--- @param opts table|nil  query opts (`sessions` to span a set of streams)
--- @return table rows
--- @return boolean truncated
function M.views.beats(session: Session, opts?: QueryOpts): {{string:any}}, boolean
    return read_view(session, BEATS_SQL, opts)
end

--- One row per answered tool call: `{ beat, call_id, name, ok }`.
---
--- The rows are rebuilt rather than edited in place, so reading a view never
--- writes to a table the caller can still be holding.
---
--- @param session userdata|table  a knl session
--- @param opts table|nil  query opts (`sessions` to span a set of streams)
--- @return table rows
--- @return boolean truncated
function M.views.tool_pairs(session: Session, opts?: QueryOpts): {{string:any}}, boolean
    local rows, truncated = read_view(session, TOOL_PAIRS_SQL, opts)
    local out: {{string:any}} = {}
    for i, row in ipairs(rows) do
        out[i] = {
            beat = row.beat,
            call_id = row.call_id,
            name = row.name,
            ok = truthy(row.ok),
        }
    end
    return out, truncated
end

--- The budget ledger: `{ seq, kind, amount, tag }` in seq order.
---
--- @param session userdata|table  a knl session
--- @param opts table|nil  query opts (`sessions` to span a set of streams)
--- @return table rows
--- @return boolean truncated
function M.views.ledger(session: Session, opts?: QueryOpts): {{string:any}}, boolean
    return read_view(session, LEDGER_SQL, opts)
end

--- The token accounting: `{ stream, calls, input_tokens, output_tokens,
--- thinking_tokens }`, one row per stream that answered.
---
--- @param session userdata|table  a knl session
--- @param opts table|nil  query opts (`sessions` to span a set of streams)
--- @return table rows
--- @return boolean truncated
function M.views.usage(session: Session, opts?: QueryOpts): {{string:any}}, boolean
    return read_view(session, USAGE_SQL, opts)
end

--- The subtree rooted at `session`: `{ session, parent, opened_epoch_ms,
--- closed_epoch_ms, open_children }`, in the order the sessions opened.
---
--- The rows are the sessions the log says were opened from this one, however
--- deep. `parent` is nil on a session whose opening recorded none — the root
--- of the whole tree, which this session is not necessarily. `closed_epoch_ms`
--- is nil while a session is still running, and `open_children` is the JSON
--- array a close recorded when it ended with children that had not (see
--- `TREE_SQL`).
---
--- `opts.sessions` is not read: which streams are in a tree is what this view
--- answers, not something a caller names. `timeout_ms` and `limit` are the
--- same knobs as anywhere else, and a subtree bigger than the limit reports
--- `truncated` like any other read.
---
--- @param session userdata|table  a knl session — the root of the walk
--- @param opts table|nil  query opts (`timeout_ms` / `limit`)
--- @return table rows
--- @return boolean truncated
function M.views.tree(session: Session, opts?: QueryOpts): {{string:any}}, boolean
    return read_view(session, TREE_SQL, opts)
end

--- Every session in the log: `{ session, meta, parent, opened_epoch_ms,
--- closed_epoch_ms }`, in the order they opened.
---
--- What tells one run from another when they share a database. `meta` is the
--- JSON text of the labels the session was opened with — `{}` for a session
--- nobody labelled — so a caller looking for one run decodes it, or asks for
--- what it wants with a `session:query` of its own; a view takes no
--- parameters, so the filtering is the caller's either way.
---
--- `closed_epoch_ms` is nil while a session is still running. `opts.sessions`
--- widens which streams are in scope, as it does on any other view.
---
--- @param session userdata|table  a knl session
--- @param opts table|nil  query opts (`sessions` / `timeout_ms` / `limit`)
--- @return table rows
--- @return boolean truncated
function M.views.sessions(session: Session, opts?: QueryOpts): {{string:any}}, boolean
    return read_view(session, SESSIONS_SQL, opts)
end

-- ============================================================
-- export — the whole log, without the table's shape
-- ============================================================
--
-- The third way of reading a log, and the one that asks the kernel for it
-- rather than the database. A view is SQL over columns, and SQL over columns
-- is a reader that knows the schema: `data` is JSON in a column, a path into
-- it is a claim about a kind's shape, and a consumer outside this repo that
-- wants "the conversation" should not have to make either claim. So this
-- reads through `session:events(from)` — the kernel's own read, whose
-- contract is a list of stored events — and hands back what it was given, or
-- what the default fold makes of it.
--
-- It is not a third TIER of read (the header's two are the kernel's built-in
-- reads and the query views). It is a caller-side composition of the first
-- one, written here because every consumer that needed it was about to write
-- the same paging loop, and one of them was going to write it without the
-- loop — which is the bug this exists to not have.

--- Every event of `session`'s own stream, read to the end.
---
--- `session:events(from)` is bounded: it answers at most the kernel's row cap
--- and says so with `truncated` beside the rows (the header). A single read
--- is therefore a PREFIX of a log that is long enough, and an export built on
--- one would silently end where the cap did — which is the failure mode that
--- looks exactly like a shorter conversation. `policy`'s `whole_log` refuses a
--- truncated read for the same reason; the difference is that a policy is
--- asking about the END of a run and cannot page to it usefully, while an
--- export wants all of it and pages until the kernel says there is no more.
---
--- Paging is by `seq`, from one past the last row of the page. A truncated
--- read with no rows to page from would be an unbounded loop, so it raises
--- instead: the kernel does not do that, and if it ever did, a hang is the
--- one way of reporting it that helps nobody.
---
--- @param session userdata|table  a knl session
--- @param who string  the caller's name, for the message
--- @return table  every stored event of the stream, in seq order
local function whole_stream(session: Session, who: string): {any}
    local out, from: {any}, kt.Seq = tag_array({}), nil
    while true do
        local rows, truncated = session:events(from)
        rows = rows or {}
        for _, ev in ipairs(rows) do
            out[#out + 1] = ev
        end
        if not truncated then
            return out
        end
        local last = rows[#rows]
        if last == nil or type(last.seq) ~= "number" then
            error(who .. ": a read that reported itself cut short answered no seq to page from", 0)
        end
        from = (last.seq as kt.Seq) + 1
    end
end

--- One stored event as a message entry, or nil for an event that is not one.
---
--- The four kinds a conversation is made of, in the same content vocabulary
--- the fold speaks — `tool_result_block` is the fold's own, shared rather
--- than restated, and an `llm_response`'s blocks are handed on verbatim the
--- way `knl.fold` hands them on. What is added is the envelope a reader
--- outside this process cannot reconstruct: which beat wrote it, where it sits
--- in the log, and when.
---
--- `tool_call` is here although the default fold SKIPS it: the call is
--- already inside its beat's `llm_response` as a `tool_use` block, so an
--- entry for it repeats a fact rather than inventing one, and a consumer
--- rebuilding a plain conversation drops the entries whose `kind` is
--- `tool_call`. What it buys is a reading in which the calls are events with
--- their own seq and their own place in time, which is what an export of a
--- log is for.
---
--- Nothing is trimmed, and there is nothing to switch off: the default fold
--- trims no content at all, and a `policy.result_cap` capped what a handler
--- ANSWERED, before the log. A `tool_result` here carries what was recorded.
local function message_entry(ev: {string:any}): {string:any}
    local kind = ev.kind
    -- An event with no `data` reads as an empty one, exactly as in `fold`: a
    -- malformed record is skipped rather than raising inside a read.
    local data = (type(ev.data) == "table" and ev.data or {}) as {string:any}
    local role, content: string, any
    if kind == "msg_user" then
        role, content = "user", data.content
    elseif kind == "llm_response" then
        role, content = "assistant", data.content
    elseif kind == "tool_call" then
        -- The same block this beat's `llm_response` already carries for the
        -- call (`knl.shapes.tool_use_block`), read back off the record the
        -- kernel wrote when it made it — not a shape invented here.
        role = "assistant"
        content = { type = "tool_use", id = data.call_id, name = data.name, input = data.args }
    elseif kind == "tool_result" then
        role, content = "user", tool_result_block(data)
    else
        return nil
    end
    local entry: {string:any} = {
        role = role,
        content = content,
        kind = kind,
        seq = ev.seq,
        epoch_ms = ev.epoch_ms,
        beat = ev.meta ~= nil and (ev.meta as {string:any}).beat or nil,
    }
    if kind == "llm_response" then
        -- The two facts an answer carries that a message does not: what the
        -- call cost and why it stopped. They ride on the entry they belong
        -- to rather than in a parallel list nobody can align.
        entry.usage = data.usage
        entry.stop_reason = data.stop_reason
    end
    return entry
end

--- The session's whole log, as events or as messages.
---
---     knl.export(s)                        -- the stored events, in seq order
---     knl.export(s, { as = "messages" })   -- the conversation they fold to
---
--- `as = "events"` (the default) hands back the objects `session:events`
--- answered, whole and unedited: `kind`, `meta`, `data`, plus the kernel's
--- stamps (`seq`, `epoch_ms`, `_schema_version`). Nothing is selected out of
--- them, which is the point — a consumer that reads a path this module does
--- not know about is reading the log, not an interface.
---
--- `as = "messages"` is one entry per event that carries a message —
--- `msg_user`, `llm_response`, `tool_call`, `tool_result` — as `{ role,
--- content, beat, seq, epoch_ms, kind }`, plus `usage` and `stop_reason` on
--- the entries that came from an `llm_response` (`message_entry`). A
--- `tool_result`'s content is never cut: the default fold trims nothing, and
--- what capped a result capped it before it was recorded.
---
--- IT IS NOT A FOLD. `knl.fold` builds one REQUEST — the messages batched and
--- ordered as a provider takes them, with a dangling `tool_use` repaired —
--- and this builds a READING, one entry per event, in seq order, repairing
--- nothing. Handing an export to a provider is not what it is for.
---
--- `sessions` names the streams to export and, this round, may name only this
--- session's own id. Reading another stream whole would mean either resuming
--- a session this call was not given (`knl.resume` is refused on a closed
--- one) or selecting the log out of the events table by hand — which is the
--- schema this reader exists not to depend on. So it is refused rather than
--- half-answered: open or resume that session and export it, or read across
--- streams with a `session:query` of your own.
---
--- The read is paged to the end (`whole_stream`), so a log longer than the
--- kernel's row cap exports whole. The session must be open; a closed one
--- raises out of the read like any other.
---
--- @param session userdata|table  a knl session (knl.open / knl.resume)
--- @param opts table|nil  `knl.shapes.export_opts` — { as?, sessions? }
--- @return table  the events, or the message entries, in seq order
function M.export(session: Session, opts?: ExportOpts): {any}
    opts = opts or {}
    if type(opts) ~= "table" then
        error("knl.export: opts must be a table", 2)
    end
    -- Loud in prod too, like every other option this module reads: a
    -- misspelled option that quietly did nothing would look exactly like the
    -- export answering what was asked for.
    -- The accepted key set is the check, so the opts are walked as the map
    -- a caller wrote.
    for key in pairs(opts as {string:any}) do
        if key ~= "as" and key ~= "sessions" then
            error("knl.export: unknown option '" .. tostring(key) .. "'", 2)
        end
    end
    local as = opts.as
    if as == nil then
        as = "events"
    end
    if as ~= "events" and as ~= "messages" then
        error('knl.export: `as` must be "events" or "messages", got ' .. tostring(as), 2)
    end
    if not is_session(session) then
        error("knl.export takes a knl session first (from knl.open / knl.resume)", 2)
    end
    if opts.sessions ~= nil then
        if type(opts.sessions) ~= "table" then
            error("knl.export: sessions must be an array of session ids", 2)
        end
        local own = session:id()
        for _, id in ipairs(opts.sessions) do
            if id ~= own then
                error(
                    "knl.export: sessions may name only this session ("
                        .. tostring(own)
                        .. ") and it named "
                        .. tostring(id)
                        .. ": exporting another stream is not in this round — open or resume that session "
                        .. "and export it, or read across streams with session:query",
                    2
                )
            end
        end
    end

    local events = whole_stream(session, "knl.export")
    if as == "events" then
        return events
    end
    local out = tag_array({})
    for _, ev in ipairs(events as {{string:any}}) do
        local entry = message_entry(ev)
        if entry ~= nil then
            out[#out + 1] = entry
        end
    end
    return out
end

-- ============================================================
-- The registry, executed
-- ============================================================
--
-- `M.shapes.api` names the shape of every argument of every export. A
-- registry nobody runs is prose with a table around it — the entry drifts
-- from the function and nothing goes red — so in dev mode each declared
-- export is replaced, once, here at load, by a wrapper that holds the call
-- to its entry before letting it through. Prod is untouched: the exports
-- are the functions themselves and a call pays nothing, which is why this
-- is a gate and not the argument checking a function needs to be correct.
-- The checks a call must not get through WITHOUT (a device's config, a
-- filter's return, cost's bound) stay where they are, loud in both modes.
--
-- What the gate judges is the shape of the arguments that were PASSED. An
-- argument that is absent — not supplied, or supplied as nil — is left to
-- the function: which arguments are required, and what a missing one means,
-- is its own business, and `knl.beat(s, nil)` must go on answering
-- `Outcome.err("conf")` rather than raising. The registry answers for what
-- it was given.
--
-- The session userdata is deliberately NOT wrapped, in either mode. Its
-- arguments are checked in Rust, on every call, by the same types
-- `knl.shapes.session` declares (`bridge/knl.rs`, `from_lua`), so a direct
-- `s:append(...)` is held to its entry without a gate here — which is the
-- hole this used to have, since a caller reaches the session straight off
-- `knl.open` and never through an export of this module. A wrapper would
-- also have to hand back a proxy in place of the kernel's value, and both
-- `local s <close> = knl.open{...}` and `knl.open{ parent = s }` want the
-- userdata itself.

--- Wrap `fn` so every argument it is passed is held to `declared[i].shape`
--- (dev mode only — `assert_dev` is a no-op otherwise, and this wrapper is
--- not even installed in prod).
---
--- The hint names the registry entry and the position, so a violation reads
--- as a broken call to a declared API rather than as an anonymous shape
--- failure somewhere inside the module.
local function arg_checked(name: string, fn: function(...: any): any..., declared: {{string:any}}): function(...: any): any...
    return function(...: any): any...
        for i = 1, #declared do
            local value = select(i, ...)
            if value ~= nil then
                -- The entry at this position: the shape the argument is
                -- held to, and the word for it.
                local declared_arg = declared[i]
                shape.assert_dev(
                    value,
                    declared_arg.shape as lshape.Schema,
                    name .. " arg " .. i .. " (" .. (declared_arg.desc as string) .. ")"
                )
            end
        end
        return fn(...)
    end
end

if shape.is_dev_mode() then
    -- The gate reaches its exports by a name the registry holds, so the
    -- module is read and written as the map of them it is — which is also
    -- what `knl/spec/api_spec.lua` walks.
    local exports = M as {string:any}
    for name, entry_any in pairs(M.shapes.api) do
        local entry = entry_any as {string:any}
        local export = exports[name]
        if type(export) == "function" then
            exports[name] = arg_checked("knl." .. name, export as function(...: any): any..., entry.args as {{string:any}})
        elseif type(export) == "table" and type(entry.members) == "table" then
            -- A namespace (`Outcome`): its members are exports too. A
            -- member of something this module does not export as a table
            -- (`device:with`) has no function here to replace, and is
            -- skipped rather than hunted down.
            local members = export as {string:any}
            for member, member_entry in pairs(entry.members as {string:any}) do
                if type(members[member]) == "function" then
                    members[member] = arg_checked(
                        "knl." .. name .. "." .. member,
                        members[member] as function(...: any): any...,
                        (member_entry as {string:any}).args as {{string:any}}
                    )
                end
            end
        end
    end
end

return M
