-- fake_bridge.lua — the one Lua stand-in for the Rust `knl` syscall bridge
-- that every spec needing a kernel is driven against.
--
-- NOT a spec: it declares no suite and calls nothing in the lspec framework,
-- which is exactly how the runner decides what to run (crates/lua-spec-runner/
-- src/main.rs, `is_spec` — detection is by USE, not by filename). A spec or a
-- spec support module reaches it with `require("knl.spec.fake_bridge")`, which
-- resolves through the same `blocks/lib` search path `require("knl")` does.
--
-- Why there is one: the bridge is not present in the pure lspec runner, so a
-- spec about anything that opens a session needs a stand-in, and each one that
-- wrote its own ended up with a slightly different kernel — one numbered
-- nothing but stamped no time, one refused a top-level `beat` and one stored
-- it, one read `timeout` as a class and one did not. The day a copy drifts, a
-- spec is passing against a kernel that does not exist. One copy, required
-- everywhere; a support module adds its own helpers on top and says what they
-- are.
--
-- How it is used: `install(opts)` builds a fresh fake and puts its syscall
-- table on the global `knl`, which is what `require("knl")` captures as its
-- syscall layer at load — so it runs BEFORE that require. It hands back the
-- fake, whose `sessions` map holds every session opened through it by id: one
-- database, as in the kernel, where a parent and its children share a file.
--
-- What it reproduces (mirroring crates/agent-block-core/src/bridge/knl.rs,
-- SESSION_API / MODULE_API, and src/knl/session.rs):
--   * the module surface is `open` / `resume` / `new_beat_id` / `error`;
--   * a session carries the WHOLE declared surface (id, scope_id, owner,
--     append, events, len, view, query, reserve, spend, remaining, exhausted,
--     close), because `knl.beat` asks for the whole surface before it treats
--     a value as a session — a stand-in answering only what a beat calls
--     would stand in for something the kernel does not hand out;
--   * `append` refuses what the kernel refuses — no kind, a top-level `beat`
--     (the id is `meta.beat`), a `meta` that is not a shallow table of
--     strings, numbers and booleans, a kind only the kernel writes — and
--     otherwise stores a copy: `seq` stamped, every other field exactly as
--     written, `meta.beat` included. The kernel stores the id it is given and
--     numbers nothing, and the budget does not move. That `meta.beat` is a
--     STRING is the Lua layer's rule, not the kernel's, so the fake takes a
--     number there: a stand-in that refused it would hide the layer's own
--     check behind one the kernel does not make;
--   * `events(from?)` answers fresh tables in the shape they were written in,
--     from `from` on, and `false` for "the row cap did not cut this short";
--   * `reserve` is the one decision point — it deducts and answers true, or
--     refuses with the grant's tag and leaves the balance where it was; no
--     budget means every reservation is allowed;
--   * `spend` deducts without asking and answers nothing (the balance is read
--     with `remaining()`);
--   * `close(reason?, detail?)` ends the session once; a second close is a
--     no-op;
--   * `open{ parent = s, budget = { from_parent = n, tag? } }` moves n out of
--     the parent's balance into the child in one move, or refuses with
--     `refused` and opens no child at all;
--   * a failure raises the attributed text `knl: <method>: <kind>: <message>`,
--     since a Rust callback cannot raise a table, and `error` reads it back;
--   * `new_beat_id` mints a fresh, time-ordered id per call, session-free.
--
-- What the caller chooses (the kernel does all of these; a spec opts into the
-- ones it has an opinion about, because each one puts rows in the log a spec
-- may be counting):
--   * `writes.lifecycle` — `session_opened{ scope_id, owner, parent? }` at
--     open and `session_closed{ reason, detail?, open_children? }` at close;
--   * `writes.ledger` — `budget_granted` at open, `budget_reserved` /
--     `budget_refused` at every reservation, `budget_spent` at every spend,
--     and the parent's side of an allocation (`child` names the child);
--   * `clock` — a function answering milliseconds, stamped as `epoch_ms` on
--     every record. Without one the fake keeps no time and a caller's
--     `epoch_ms` passes through, so a spec can write history rather than wait
--     for it;
--   * `query` — what a SELECT answers: `function(session, sql, params, opts)
--     -> rows, truncated`. Without one, a query answers `session._query_rows`
--     (empty until a case puts rows there) and `false`.
--
-- What it does not do, named so no case mistakes it for the real thing:
--   * no SQLite stands behind `query`: the call is recorded in
--     `session._queries` as `{ sql, params, opts }`, and what a statement
--     SELECTS is asked where there is a database
--     (crates/agent-block/tests/fixtures/knl_beat_test.lua);
--   * `store` is recorded, not honoured — the log is in memory and is the
--     session itself;
--   * `view` folds nothing: every name is unknown here, and the method is
--     carried because the surface has it;
--   * no `api`: the declared surface is the kernel's to report, and a copy
--     written here would be a third declaration to keep in step. The module's
--     registry is checked without a bridge (knl/spec/api_spec.lua) and against
--     the real one where there is one (knl_beat_test.lua, inv10).
--
-- The state a spec may read or set directly, and nothing else: `_events` (the
-- stored records), `_queries`, `_query_rows`, `closed`, `close_reason`,
-- `resumed_from`.

local M = {}

--- The classes the kernel publishes (Rust `KnlError::KINDS`). Retyped here on
--- purpose: this table stands in for the bridge, and a stand-in that borrowed
--- the module's own list could not catch the module reading it wrong. The two
--- are held against each other where a real bridge exists
--- (tests/fixtures/knl_beat_test.lua, inv10).
M.ERROR_KINDS = {
    busy = true,
    storage = true,
    corruption = true,
    closed = true,
    validation = true,
    unsupported = true,
    timeout = true,
    refused = true,
}

--- The kinds only the kernel writes (Rust `is_kernel_only`): an append of one
--- is refused, whoever wrote the shape right.
M.KERNEL_KINDS = {
    session_opened = true,
    session_closed = true,
    budget_granted = true,
    budget_reserved = true,
    budget_refused = true,
    budget_spent = true,
}

--- `knl.error` as the bridge implements it (bridge/knl.rs `error_table`): the
--- raise is text, `knl: <method>: <kind>: <message>`, because mlua cannot
--- carry a table out of a Rust callback. Only a class the kernel publishes is
--- read as one; anything else comes back whole and unclassified, and the table
--- renders as the message it was read from.
function M.read_error(raised)
    local text = tostring(raised)
    local out = { message = text, retryable = false }
    for line in text:gmatch("[^\n]+") do
        local attributed = line:match("knl: (.+)$")
        if attributed then
            -- Non-greedy: the first two separators only, like Rust's two
            -- `split_once(": ")` — the message keeps its own colons.
            local method, kind, message = attributed:match("^(.-): (.-): (.*)$")
            if kind ~= nil and M.ERROR_KINDS[kind] then
                out.method, out.kind, out.message = method, kind, message
                out.retryable = kind == "busy"
            end
            break
        end
    end
    return setmetatable(out, {
        __tostring = function()
            return text
        end,
    })
end

--- Raise the way the bridge does: attributed text, no position prefix.
local function raise(method, kind, message)
    error(string.format("knl: %s: %s: %s", method, kind, message), 0)
end

local function copy(value)
    if type(value) ~= "table" then
        return value
    end
    local out = {}
    for k, v in pairs(value) do
        out[k] = copy(v)
    end
    return out
end

local function amount_of(method, n)
    if type(n) ~= "number" or n < 0 then
        raise(method, "validation", "amount must be a non-negative number")
    end
    return n
end

--- A fresh fake: its own counters, its own sessions, nothing installed.
---
--- @param opts table|nil  { writes = { lifecycle?, ledger? }, clock?, query? }
--- @return table  { bridge = <the syscall table>, sessions = { [id] = session } }
function M.new(opts)
    opts = opts or {}
    local writes = opts.writes or {}
    local clock = opts.clock
    local answer = opts.query

    local fake = { sessions = {} }
    local opened, minted = 0, 0

    -- The store: a copy of what was written, `seq` stamped, and `epoch_ms`
    -- only when this fake keeps time.
    local function record(s, event)
        local stored = copy(event)
        s._seq = s._seq + 1
        stored.seq = s._seq
        if clock ~= nil then
            stored.epoch_ms = clock()
        end
        s._events[#s._events + 1] = stored
        return stored.seq
    end

    -- A write the kernel makes on its own, when the caller asked for that
    -- family of them.
    local function kernel_write(s, family, kind, data)
        if writes[family] then
            record(s, { kind = kind, data = data })
        end
    end

    local function new_session(id, owner)
        local s = {
            _events = {},
            -- One entry per `query` call: { sql, params, opts }.
            _queries = {},
            -- What every `query` answers without a `query` option, until a
            -- case puts rows here.
            _query_rows = {},
            _children = {},
            _seq = 0,
            _owner = owner,
            closed = false,
            close_reason = nil,
        }

        -- Identity, as the bridge answers it: three methods, not three fields.
        function s:id()
            return id
        end
        function s:scope_id()
            return "scope-" .. id
        end
        function s:owner()
            return self._owner
        end

        function s:append(event)
            if self.closed then
                raise("append", "closed", "session is closed")
            end
            if type(event) ~= "table" then
                raise("append", "validation", "event must be a table")
            end
            if type(event.kind) ~= "string" then
                raise("append", "validation", "kind is required (string)")
            end
            if event.beat ~= nil then
                raise("append", "validation", "unknown field: beat (the id is meta.beat)")
            end
            local meta = event.meta
            if meta ~= nil then
                if type(meta) ~= "table" then
                    raise("append", "validation", "meta must be a table")
                end
                for key, value in pairs(meta) do
                    local t = type(value)
                    if t ~= "string" and t ~= "number" and t ~= "boolean" then
                        raise(
                            "append",
                            "validation",
                            "meta is shallow: " .. tostring(key) .. " must be a string, a number or a boolean"
                        )
                    end
                end
            end
            if M.KERNEL_KINDS[event.kind] then
                raise("append", "validation", event.kind .. " is written by the kernel only")
            end
            return record(self, event)
        end

        function s:events(from)
            local out = {}
            for _, event in ipairs(self._events) do
                if from == nil or event.seq >= from then
                    out[#out + 1] = copy(event)
                end
            end
            return out, false
        end

        function s:len()
            return #self._events
        end

        function s:view(_name, _opts)
            raise("view", "validation", "unknown view")
        end

        function s:query(sql, params, query_opts)
            if type(sql) ~= "string" then
                raise("query", "validation", "sql must be a string")
            end
            self._queries[#self._queries + 1] = { sql = sql, params = params, opts = query_opts }
            if answer ~= nil then
                return answer(self, sql, params, query_opts)
            end
            return self._query_rows, false
        end

        function s:reserve(n)
            if self.closed then
                raise("reserve", "closed", "session is closed")
            end
            amount_of("reserve", n)
            if self._remaining == nil then
                return true
            end
            if self._remaining < n then
                kernel_write(self, "ledger", "budget_refused", {
                    amount = n,
                    tag = self._tag,
                    remaining = self._remaining,
                })
                return false, self._tag
            end
            self._remaining = self._remaining - n
            kernel_write(self, "ledger", "budget_reserved", { amount = n, tag = self._tag })
            return true
        end

        function s:spend(n)
            if self.closed then
                raise("spend", "closed", "session is closed")
            end
            amount_of("spend", n)
            if self._remaining == nil then
                return
            end
            self._remaining = math.max(0, self._remaining - n)
            kernel_write(self, "ledger", "budget_spent", { amount = n, tag = self._tag })
        end

        function s:remaining()
            return self._remaining
        end

        function s:exhausted()
            return self._remaining ~= nil and self._remaining <= 0
        end

        function s:close(reason, detail)
            if self.closed then
                return
            end
            self.closed = true
            self.close_reason = reason or "closed"
            local still = {}
            for _, child in ipairs(self._children) do
                if not child.closed then
                    still[#still + 1] = child:id()
                end
            end
            kernel_write(self, "lifecycle", "session_closed", {
                reason = self.close_reason,
                detail = detail,
                open_children = #still > 0 and still or nil,
            })
        end

        return s
    end

    local function open(o)
        o = o or {}
        local parent = o.parent
        local budget = o.budget or {}
        opened = opened + 1
        local id = string.format("sess-%06d", opened)
        local owner = o.owner or (parent ~= nil and parent._owner) or "anon"

        if parent ~= nil then
            -- The allocation: one move, both ledgers, or neither.
            local amount = amount_of("open", budget.from_parent)
            local tag = budget.tag or parent._tag
            if parent._remaining ~= nil and parent._remaining < amount then
                kernel_write(parent, "ledger", "budget_refused", {
                    amount = amount,
                    tag = tag,
                    remaining = parent._remaining,
                    child = id,
                })
                raise(
                    "open",
                    "refused",
                    "the parent's balance is "
                        .. tostring(parent._remaining)
                        .. ", which does not cover "
                        .. tostring(amount)
                )
            end
            if parent._remaining ~= nil then
                parent._remaining = parent._remaining - amount
            end
            local s = new_session(id, owner)
            s._remaining = amount
            s._tag = tag
            s._parent = parent
            s._store = o.store
            fake.sessions[id] = s
            parent._children[#parent._children + 1] = s
            kernel_write(parent, "ledger", "budget_reserved", { amount = amount, tag = tag, child = id })
            kernel_write(s, "lifecycle", "session_opened", {
                scope_id = "scope-" .. id,
                owner = owner,
                parent = parent:id(),
            })
            kernel_write(s, "ledger", "budget_granted", { amount = amount, tag = tag, parent = parent:id() })
            return s
        end

        local s = new_session(id, owner)
        s._remaining = budget.amount
        s._tag = budget.tag
        s._store = o.store
        fake.sessions[id] = s
        kernel_write(s, "lifecycle", "session_opened", { scope_id = "scope-" .. id, owner = owner })
        if budget.amount ~= nil then
            kernel_write(s, "ledger", "budget_granted", { amount = budget.amount, tag = budget.tag })
        end
        return s
    end

    fake.bridge = {
        open = open,
        -- A resumed stream is a fresh handle here; which stream it names is
        -- kept so a case can see it asked for the right one.
        resume = function(o)
            local s = open(o)
            s.resumed_from = o and o.session
            return s
        end,
        error = M.read_error,
        -- Time-ordered and session-free, like the Rust UUID v7 mint.
        new_beat_id = function()
            minted = minted + 1
            return string.format("beat-%06d", minted)
        end,
    }

    return fake
end

--- Build a fresh fake and install its syscall table as the global `knl`.
--- Call it BEFORE `require("knl")`: the module captures the global at load.
---
--- @param opts table|nil  as `new`
--- @return table  the fake (`bridge`, `sessions`)
function M.install(opts)
    local fake = M.new(opts)
    knl = fake.bridge
    return fake
end

return M
