--- host_types — what the host puts in a VM's globals, declared for the checker.
---
--- A module written in Teal (`lib/<name>/init.tl`) reads the same globals a
--- Lua one does: `std.*` from mlua-batteries, and the bridges beside it. The
--- checker has to be told their shape, and told in a way that generates no
--- code — the global is the host's, set before any module loads, and a module
--- that assigned it would be the second owner of one name. So the shape is a
--- record here, and a `.tl` declares the global against it:
---
---     local host = require("host_types")
---     global std: host.Std
---
--- `global std: host.Std` is a declaration, not an assignment: the generated
--- Lua carries nothing for it, and `std.json.encode` in the module reads the
--- global at call time, late-bound, exactly as the Lua module did — which is
--- what lets a spec install a fake `std` before requiring the module.
---
--- The `require` line is what the generated Lua does carry, so this
--- declaration has a run-time half beside it, `host_types.lua`, an empty
--- table: the checker reads the `.d.tl`, the run loads the `.lua` (the host
--- embeds it, and `vendor` writes it), and neither is confused for the other.
---
--- What is declared is what a Teal module has needed so far, not the whole of
--- `std`: a field arrives with the first module that reads it. A name missing
--- here is a type error in that module, which is the right place to learn it.
local record host_types
    --- `std`, as registered by `mlua_batteries::register_all` in `host.rs`,
    --- with `std.kv` from the sqlite batteries behind the `sqlite` feature.
    record Std
        record Json
            --- JSON text for a value.
            encode: function(any): string
        end
        --- Namespace / key → JSON-serialised value, in the host's database.
        record Kv
            --- The value under (ns, key), or nil.
            get: function(string, string): any
            --- Write a value under (ns, key), replacing what was there.
            set: function(string, string, any)
            --- Remove (ns, key); true when a row was there to remove.
            delete: function(string, string): boolean
            --- The keys under ns, those with the prefix when one is given.
            list: function(string, ?string): {string}
        end
        --- The SQL bridge over the host's SQLite database (`bridge/sql.rs`).
        record Sql
            record ExecResult
                affected: integer
                last_id: integer
            end
            --- Rows of a SELECT, each a column → value table.
            query: function(string, ?{any}): {{string:any}}
            --- A statement; how many rows it touched and the last rowid.
            exec: function(string, ?{any}): ExecResult
            --- The sentinel a NULL column comes back as.
            null: any
        end
        --- The time-series bridge over the host's SQLite database
        --- (`bridge/ts.rs`): named streams of timestamped JSON values.
        record Ts
            --- Append a point: series, value, optional flat tags, optional
            --- Unix-ms timestamp (now when absent).
            append: function(string, any, ?{string:string}, ?integer)
            --- Rows of a series under the query options (`from` / `to` /
            --- `tags` / `agg` / `bucket_ms` / `limit` / `offset`).
            query: function(string, ?{string:any}): {{string:any}}
            --- The latest row matching the tags, or nil.
            last: function(string, ?{string:string}): {string:any}
        end
        --- The file bridge (`bridge/fs.rs`): versioned reads, line-addressed
        --- edits held to a `base` version, and the rollback of the last one.
        record Fs
            record ReadVersioned
                content: string
                version: string
                lines: integer
            end
            --- The file, its content fingerprint, and its line count.
            read_versioned: function(string): ReadVersioned
            --- `{ base?, edits = {...} }`; answers the bridge's result table.
            edit: function(string, {string:any}): {string:any}
            --- Write the whole file.
            write: function(string, string)
            --- Restore the content the last successful edit replaced.
            rollback: function(string): {string:any}
        end
        json: Json
        kv: Kv
        sql: Sql
        ts: Ts
        fs: Fs
    end
    --- `tool`, the tool registry bridge (`bridge/tool.rs`): what a model may
    --- call, by name.
    record Tool
        record Meta
            description: string
            input_schema: {string:any}
        end
        --- Register one tool: its name, what it takes, and the handler.
        register: function(string, Meta, function(any): any)
    end
    --- `log`, the tracing bridge (`bridge/log.rs`): one message per call,
    --- attributed to the running script.
    record Log
        debug: function(string)
        info: function(string)
        warn: function(string)
        error: function(string)
    end
end
return host_types
