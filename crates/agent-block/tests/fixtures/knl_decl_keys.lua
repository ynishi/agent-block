-- Prints the keys the `knl` module exports, one `KEYS <table>=a,b,c` line
-- per table `knl.d.tl` declares a record for — the module itself (its
-- functions and tables), `knl.views`, `knl.shapes`, `knl.Outcome`.
-- tests/e2e_knl_decl_drift.rs reads the declaration and holds it to these
-- lines: a name declared that the module does not export is a declaration
-- that has drifted.
local knl = require("knl")
local function keys_of(label, t, only_functions)
    local names = {}
    for k, v in pairs(t) do
        if type(k) == "string" and (not only_functions or type(v) == "function") then
            names[#names + 1] = k
        end
    end
    table.sort(names)
    print("KEYS " .. label .. "=" .. table.concat(names, ","))
end
keys_of("knl", knl, false)
keys_of("knl.views", knl.views, true)
keys_of("knl.shapes", knl.shapes, false)
keys_of("knl.Outcome", knl.Outcome, true)
