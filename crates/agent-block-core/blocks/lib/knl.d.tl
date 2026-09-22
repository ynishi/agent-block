--- knl — the Lua kernel, declared for the checker.
---
--- The module beside this (`knl/init.lua`) is Lua, and stays Lua for now:
--- this is what a module written in Teal is checked against when it
--- requires `knl` — the "runtime, typed by" state, where the `.lua` runs and
--- the `.d.tl` types it. The day `knl` is written in Teal these records move
--- into it and this file goes; until then a name declared here that the
--- module does not export is what `crates/agent-block/tests/
--- e2e_knl_decl_drift.rs` catches.
---
--- What is declared is the surface the modules above this one read —
--- supervisor, policy, knl_adapter, the two blocks — in the shapes
--- `knl.shapes` publishes as lshape for run time (`knl/init.lua`, the
--- `M.shapes` block). Where lshape says `discriminated("status", …)` this
--- says one record with every arm's fields, because a Teal union of records
--- needs a `where` on each and the readers here test `status` and read the
--- field they know follows; the arms are documented on the record. The
--- syscall types the kernel's Rust side declares — the session's `append`
--- takes an `AppendEvent`, its `query` a `QueryOpts` — come from
--- `knl_types.d.tl`, generated from those types.
local kt = require("knl_types")
local lshape = require("lshape")

local record knl
    --- A kernel session: the durable half. What the kernel's session
    --- userdata answers (`bridge/knl.rs`); a fake answers the same.
    record Session
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
    record ToolSpec
        name: string
        description: string
        input_schema: any
        handler: function(any): any
    end
    --- The entry a device's map holds under the tool's name.
    record ToolEntry
        description: string
        input_schema: any
        handler: function(any): any
    end
    --- What a model call answers (`knl.shapes.llm_usage`).
    record Usage
        input_tokens: number
        output_tokens: number
        thinking_tokens: number
    end
    --- A model call's answer (`knl.shapes.llm_result`): `status` is `ok` or
    --- `refused`; `refusal` is set on the second.
    record LlmResult
        status: string
        content: {any}
        usage: Usage
        stop_reason: string
        refusal: any
    end
    --- A failed model call, classified (`knl.shapes.call_error`).
    record CallError
        kind: string
        retryable: boolean
        retry_after: number
        message: string
        status: number
    end
    --- What a fold answers: the request a model call carries.
    record Request
        messages: {any}
        system: any
        tools: {any}
    end
    --- What a fold answers beside the request when it left something out
    --- (`knl.shapes.window_report`).
    record WindowReport
        dropped: {string}
        kept: number
        seed_kept: boolean
        before: number
        after: number
        limit: number
        reserve: number
    end
    --- What `knl.device` takes (`knl.shapes.device_config`).
    record DeviceConfig
        llm: any
        tools: {string:ToolEntry}
        tool_policy: function(any, any, any): any
        fold: function({any}, Device): Request
        filters: {function(any): any}
        system: any
        cost: function(any): number
    end
    --- The policy half: a frozen value with the resolved fields, and `with`
    --- for a derived one.
    record Device
        llm: any
        tools: {string:ToolEntry}
        tool_policy: function(any, any, any): any
        fold: function({any}, Device): Request
        filters: {function(any): any}
        system: any
        cost: function(any): number
        with: function(Device, DeviceConfig): Device
    end
    --- What a beat answers (`knl.shapes.outcome`): `status` is one of `ok`
    --- (`out`), `refused` (`reason`, `detail`), `error` (`kind`, `detail`),
    --- `stopped` (`reason`, `tag`).
    record OutcomeValue
        status: string
        out: any
        reason: string
        detail: any
        kind: string
        tag: string
    end
    --- The judgements on an outcome, and the exhaustive match — `knl.Outcome`
    --- (the record is named apart from the field, which Teal asks for).
    record OutcomeApi
        is_ok: function(any): boolean
        is_refused: function(any): boolean
        is_error: function(any): boolean
        is_stopped: function(any): boolean
        match: function(OutcomeValue, {string:function(OutcomeValue): any}): any
    end
    --- What `knl.error` reads a raised syscall into (`knl.shapes.error`).
    record Error
        kind: string
        method: string
        retryable: boolean
        message: string
        cause: string
    end
    --- What `knl.open` takes.
    record OpenOpts
        owner: string
        budget: any
        store: any
        meta: any
        parent: any
    end
    --- What `knl.resume` takes.
    record ResumeOpts
        store: any
        session: string
        budget: any
    end
    --- What a view or an export takes: the streams to span, and the read's
    --- bounds.
    record QueryOpts
        sessions: {string}
        timeout_ms: number
        limit: number
    end
    record ExportOpts
        as: string
        sessions: {string}
    end
    --- The kernel's views: rows, and whether the read was cut.
    record Views
        beats: function(Session, ?QueryOpts): {{string:any}}, boolean
        tool_pairs: function(Session, ?QueryOpts): {{string:any}}, boolean
        ledger: function(Session, ?QueryOpts): {{string:any}}, boolean
        usage: function(Session, ?QueryOpts): {{string:any}}, boolean
        tree: function(Session, ?QueryOpts): {{string:any}}, boolean
        sessions: function(Session, ?QueryOpts): {{string:any}}, boolean
    end
    --- The contracts, as data: lshape shapes a pack asserts at its own
    --- boundary in dev mode, and the closed lists beside the shapes that
    --- close on them.
    record Shapes
        outcome: lshape.Schema
        error: lshape.Schema
        query_opts: lshape.Schema
        schema: any
        error_kinds: {string}
        call_error: lshape.Schema
        call_error_kinds: {string}
        call_error_retryable: {string:boolean}
        request: lshape.Schema
        window_report: lshape.Schema
        export_opts: lshape.Schema
        event_base: lshape.Schema
        event_meta: lshape.Schema
        events: {string:lshape.Schema}
        budget_allocation: lshape.Schema
        device_config: lshape.Schema
        tool_spec: lshape.Schema
        tool_entry: lshape.Schema
        tool_policy_decision: lshape.Schema
        cost_result: lshape.Schema
        llm_result: lshape.Schema
        llm_usage: lshape.Schema
        tool_use_block: lshape.Schema
        session_handle: lshape.Schema
        callable: lshape.Schema
        open_opts: lshape.Schema
        resume_opts: lshape.Schema
        budget_grant: lshape.Schema
        --- The registry: one entry per export, naming what goes in and out.
        api: {string:any}
        session: lshape.Schema
    end

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
return knl
