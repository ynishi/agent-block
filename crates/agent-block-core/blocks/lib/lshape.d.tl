--- lshape — the vendored validator (`blocks/lib/lshape/`, mlua-lshape 0.3),
--- declared for the checker.
---
--- The `.lua` beside this is upstream's, copied verbatim and not forked;
--- this file is what a Teal module sees when it requires `lshape`, and it
--- declares the part of the surface such a module uses — the `t` DSL a
--- shape is written in and the `check` a boundary is asserted with. A name
--- missing here is added with the module that first needs it.
---
--- A tool module requires it tolerantly, `pcall(require, "lshape")`, so
--- that the bridge's fallback (a bare VM where nothing resolves) can still
--- load the module; the checker reads the declaration either way.
local record lshape
    --- One schema. Every combinator answers one, and the chained methods
    --- answer the same schema with the annotation added.
    record Schema
        is_optional: function(Schema): Schema
        describe: function(Schema, string): Schema
        min: function(Schema, number): Schema
        max: function(Schema, number): Schema
        min_len: function(Schema, number): Schema
        max_len: function(Schema, number): Schema
    end
    --- What `shape` / `partial` take beside the fields.
    record ShapeOpts
        open: boolean
    end
    --- The DSL, `lshape.t`.
    record T
        string: Schema
        number: Schema
        boolean: Schema
        table: Schema
        fn: Schema
        any: Schema
        integer: Schema
        shape: function({string:Schema}, ?ShapeOpts): Schema
        partial: function({string:Schema}, ?ShapeOpts): Schema
        array_of: function(Schema): Schema
        one_of: function({any}): Schema
        literal: function(any): Schema
        discriminated: function(string, {string:Schema}): Schema
        pattern: function(string): Schema
        ref: function(string): Schema
        any_of: function({Schema}): Schema
        tuple: function({Schema}): Schema
        map_of: function(Schema, Schema): Schema
        --- The plain-data form a schema is built from, for a prim `t`
        --- does not name (a function).
        _internal: Internal
    end
    record Internal
        schema_mt: metatable<any>
    end
    --- The boundary checks, `lshape.check`.
    record Check
        check: function(any, any, ?any): boolean, any
        assert: function(any, any, ?string, ?any)
        --- The dev-mode assert: a no-op unless `LSHAPE_CHECK=1`.
        assert_dev: function(any, any, ?string, ?any)
        is_dev_mode: function(): boolean
        dev_env_var: string
    end
    t: T
    check: Check
    _VERSION: string
end
return lshape
