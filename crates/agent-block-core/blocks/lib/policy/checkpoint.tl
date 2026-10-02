--- policy.checkpoint — the state of the files, read back off the record.
---
--- Two readings, on a file of their own because they read the one thing the
--- rest of the log does not hold: what the files WERE. The log has the
--- conversation, the decisions, every tool call and what it answered — but a
--- file's content after beat N is nowhere in it, so a run that went wrong
--- could be read back move by move and still not say what it left on disk at
--- any point. The `checkpoint` record is that missing half, and these two
--- read it:
---
---     policy.checkpoints(source)       -> { { beat, seq, files, missing }, ... }
---                                         every recorded state, in log order;
---                                         `files` maps a path to its version
---     policy.checkpoint_at(source, beat?) -> { beat, files, missing } | nil
---                                         one state, `files` mapping a path
---                                         to its CONTENT: the beat named, or
---                                         the latest when none is
---
--- Neither appends — the rule every file here keeps. The `checkpoint` and
--- `checkpoint_blob` events are the LOOP's to append (`coding`'s
--- `checkpoint` knob does it: after every beat that landed an edit, and once
--- before the first beat), exactly as `summary` is; `policy.shapes.checkpoint`
--- and `policy.shapes.checkpoint_blob` say the shapes it appends. Writing the
--- files back is the caller's too (`coding.restore`), because a reader that
--- wrote to disk would be a loop pretending to be a value.
---
--- Readings, not policies: neither plugs into a seam of the device, and the
--- loop does not consult them between beats. They sit beside the fifteen the
--- way `policy.split` does — for a caller reading a run, during it or after.
---
--- The record they read
---   A `checkpoint` is the state of EVERY target at one point, not only the
---   files the beat edited, so one event is the whole state and no reader has
---   to replay the ones before it. It names each file by `version` — the
---   fingerprint `std.fs.read_versioned` answers (the first 16 hex digits of
---   the content's SHA-256, the same value an `edit`'s `base` is checked
---   against) — and the content itself is in a `checkpoint_blob`, written
---   once per distinct version per session. A file the edit did not touch
---   names the version it already had and costs one map entry; a content the
---   run returns to (an edit undone) is not written twice.
---
---   The beat a state follows is the envelope's `meta.beat`, the same label
---   every event of that beat carries — the kernel takes it on any append, and
---   it puts the state in the beat record of the beat that produced it. The
---   state before the first beat carries no `meta.beat` and reads back as
---   `"baseline"` (`shared.CHECKPOINT_BASELINE` says why it carries none). A
---   target that could not be read as text at that point — not created yet,
---   deleted, binary — is listed under `missing` rather than left out, so
---   "absent then" is a fact on record and not a gap.
---
--- What the log argument is
---   A knl session, read whole — and a truncated read is refused, as every
---   reader here refuses one (`shared.whole_log`): a checkpoint cut off the
---   end of the log is exactly the newest one, and `checkpoint_at` with no
---   beat would answer an older state as the latest. OR the events of a
---   session, in seq order: what `knl.export(s)` answers, or the lines of
---   `agent-block knl export --as events`, each decoded. The second form is
---   what reaches a finished run: its session is closed, and the kernel does
---   not resume a closed stream, so after `coding.run` returns its record is
---   read through an export. It is also the form a replay fixture holds
---   (`policy/spec/replay/`), which is where this record is meant to go next:
---   a case whose decision depends on the files can carry them as recorded
---   checkpoints instead of rebuilding them by hand.
---
--- What the content is NOT: anything outside the targets. A verify writes
--- build artifacts, caches and lock files wherever it likes; none of that is
--- recorded, and nothing here can say what it was.
---
--- Exported through `policy`; see that header for the rules every policy
--- keeps. This file is `require`d by `policy/init.lua` and not meant to be
--- reached for directly.

local kernel = require("knl")
local lshape = require("lshape")
local shared = require("policy.shared")
local T = lshape.t

local record M
    --- One recorded state, as `policy.checkpoints` lists it: path -> version.
    record Checkpoint ---@struct
        beat: string
        seq: integer ---@optional
        files: {string:string}
        missing: {string}
    end
    --- One recorded state, resolved: path -> content.
    record State ---@struct
        beat: string
        files: {string:string}
        missing: {string}
    end
    type CheckpointsFn = function(any): {Checkpoint}
    type CheckpointAtFn = function(any, ?string): State
    checkpoints: CheckpointsFn
    checkpoint_at: CheckpointAtFn
    shapes: {string:lshape.Schema}
    api: {string:shared.ApiEntry}
end

-- What this file shares with its siblings, by the names the code was
-- written against (`policy.shared`).
local CHECKPOINT_KIND = shared.CHECKPOINT_KIND
local BLOB_KIND = shared.CHECKPOINT_BLOB_KIND
local BASELINE = shared.CHECKPOINT_BASELINE
local ARRAY_TAG = shared.ARRAY_TAG
local whole_log = shared.whole_log
local beat_of = shared.beat_of
local data_of = shared.data_of
local arg_of = shared.arg_of

--- An array that encodes as one even when it is empty: a block that hands
--- these readings to `std.json.encode` gets `[]`, not `{}`.
local function array<V>(): {V}
    return setmetatable({} as {V}, ARRAY_TAG as metatable<{V}>)
end

--- The events of `source`, whole: a session read through `whole_log`, or an
--- array of events taken as it is.
---
--- @param source any  a knl session, or its events in seq order
--- @param who string  the reader's name, for the messages
--- @return table  the events, in seq order
local function events_of(source: any, who: string): {shared.Event}
    if kernel.is_session(source) then
        return whole_log(source as kernel.Session, who)
    end
    if type(source) == "table" then
        for i, ev in ipairs(source as {any}) do
            if type(ev) ~= "table" then
                error(who .. ": log[" .. tostring(i) .. "] is not an event (" .. type(ev) .. ")", 3)
            end
        end
        return source as {shared.Event}
    end
    error(
        who
            .. ": takes a knl session, or the events of one in seq order (knl.export(s), or the lines "
            .. "of `agent-block knl export --as events` decoded), got "
            .. type(source),
        3
    )
end

--- One `checkpoint` event, read: the beat it follows, where it is in the
--- log, and the two halves of its data. A malformed record reads as an
--- empty state rather than being indexed, as every reader here does.
local function point_of(ev: shared.Event): M.Checkpoint
    local data = data_of(ev)
    local files: {string:string} = {}
    if type(data.files) == "table" then
        for path, version in pairs(data.files as {any:any}) do
            if type(path) == "string" and type(version) == "string" then
                files[path as string] = version as string
            end
        end
    end
    local missing = array() as {string}
    if type(data.missing) == "table" then
        for _, path in ipairs(data.missing as {any}) do
            if type(path) == "string" then
                missing[#missing + 1] = path as string
            end
        end
    end
    local seq = ev.seq
    return {
        beat = beat_of(ev) or BASELINE,
        seq = type(seq) == "number" and seq as integer or nil,
        files = files,
        missing = missing,
    }
end

--- Every recorded state of the files, in log order.
---
---     for _, point in ipairs(policy.checkpoints(session)) do
---         print(point.beat, point.files["/repo/src/lib.rs"])
---     end
---
--- Each is `{ beat, seq, files, missing }`: the beat it follows
--- (`"baseline"` for the state before the first one), its `seq` when the
--- event carries one, path -> version, and the targets that could not be
--- read then. Versions, not contents — a listing of a long run should not
--- carry every file it ever held; `checkpoint_at` resolves one state.
---
--- @param source any  a knl session, or its events in seq order
--- @return table  the states, in log order (empty when none was recorded)
function M.checkpoints(source: any): {M.Checkpoint}
    local out = array() as {M.Checkpoint}
    for _, ev in ipairs(events_of(source, "policy.checkpoints")) do
        if ev.kind == CHECKPOINT_KIND then
            out[#out + 1] = point_of(ev)
        end
    end
    return out
end

--- One recorded state of the files, with the contents: the one that
--- followed `beat`, or the latest when `beat` is nil.
---
---     local state = policy.checkpoint_at(session, "baseline")
---     -- state.files["/repo/src/lib.rs"] is that file's content before the first beat
---
--- Answers `{ beat, files = { [path] = content }, missing = { path, ... } }`,
--- or nil when the log holds no such state — no checkpoint at all, or none
--- after that beat (a beat that landed no edit records none).
---
--- A version a checkpoint names with no `checkpoint_blob` carrying it is
--- RAISED, not skipped: an answer with a file quietly missing would be
--- written back as a state the run never had. The loop writes every blob
--- before the checkpoint naming it, so a whole log never has one; a hand-cut
--- one can.
---
--- @param source any  a knl session, or its events in seq order
--- @param beat string|nil  the beat id, or `"baseline"`; nil for the latest
--- @return table|nil  the state, or nil when there is none
function M.checkpoint_at(source: any, beat?: string): M.State
    local who = "policy.checkpoint_at"
    if beat ~= nil and type(beat) ~= "string" then
        error(who .. ": beat must be a beat id (a string) or nil for the latest, got " .. type(beat), 2)
    end
    local events = events_of(source, who)
    local point: M.Checkpoint = nil
    local contents: {string:string} = {}
    for _, ev in ipairs(events) do
        if ev.kind == CHECKPOINT_KIND then
            local p = point_of(ev)
            if beat == nil or p.beat == beat then
                point = p
            end
        elseif ev.kind == BLOB_KIND then
            local data = data_of(ev)
            if type(data.version) == "string" and type(data.content) == "string" then
                contents[data.version as string] = data.content as string
            end
        end
    end
    if point == nil then
        return nil
    end
    local files: {string:string} = {}
    for path, version in pairs(point.files) do
        local content = contents[version]
        if content == nil then
            error(
                who
                    .. ": the checkpoint after beat "
                    .. point.beat
                    .. " records "
                    .. path
                    .. " at version "
                    .. version
                    .. ", and no checkpoint_blob in the log carries that version — the log is not whole",
                2
            )
        end
        files[path] = content
    end
    return { beat = point.beat, files = files, missing = point.missing }
end

--- The two arguments, as the registry declares them. Both are `any` on
--- purpose: what either may be is judged by the reader itself, loud in both
--- modes (`events_of`, and `checkpoint_at`'s beat check), and a narrower
--- shape here would have the dev-mode gate answer first, in other words —
--- the split `shared.opts_contract` makes for opts, made here for the two
--- positional arguments that have no opts table to make it in.
local LOG_ARG = arg_of(T.any, "log — a knl session, or its events in seq order")
local BEAT_ARG = arg_of(T.any, 'beat — an id, or "baseline"; nil for the latest')

--- The contracts this file publishes; `policy.shapes` gathers them.
M.shapes = {
    checkpoint = shared.CHECKPOINT,
    checkpoint_blob = shared.CHECKPOINT_BLOB,
}

--- This file's entries in `policy.shapes.api`, gathered there.
M.api = {
    checkpoints = {
        args = { LOG_ARG },
        returns = "{ { beat, seq?, files = { [path] = version }, missing = { path ... } } ... }, in log order",
    },
    checkpoint_at = {
        args = { LOG_ARG, BEAT_ARG },
        returns = "nil | { beat, files = { [path] = content }, missing = { path ... } }",
    },
}

return M
