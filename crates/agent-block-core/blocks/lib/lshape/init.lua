--- lshape — Schema-as-Data validator + LuaCATS codegen (Pure Lua).
---
--- Aggregate module that re-exports the four core sub-modules:
---   t        — DSL combinators (T.string / T.shape / T.array_of / ...)
---   check    — validator (check / assert / assert_dev / is_dev_mode)
---   reflect  — read-only reflection over shape schemas
---   luacats  — LuaCATS `---@class` codegen
---
--- Typical use:
---   local lshape  = require("lshape")
---   local T       = lshape.t
---   local check   = lshape.check
---   local Voted   = T.shape({ answer = T.string, reasoning = T.string })
---   local ok, why = check.check(value, Voted)
---
--- Host bindings (see check.lua / luacats.lua for contracts):
---   check.default_registry = <{name = schema}>   -- H1 (default nil)
---   check.dev_env_var      = "MY_APP_CHECK"      -- H2 (default "LSHAPE_CHECK")
---   luacats.gen(shapes, "MyAppPrefix")           -- H3 (default "")

local M = {}

M._VERSION = "0.1.0"

M.t = require("lshape.t")
M.check = require("lshape.check")
M.reflect = require("lshape.reflect")
M.luacats = require("lshape.luacats")

return M
