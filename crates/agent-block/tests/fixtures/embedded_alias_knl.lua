-- embedded_alias_knl.lua — `embedded.<name>` reaches the kernel too.
--
-- A project that replaces `knl` still reads the embedded one here, which is
-- what an override needs to wrap what it replaced. The alias is also the
-- assertion that `embedded.` resolves from memory only: one of the tests puts
-- a decoy at `<project>/lib/embedded/knl.lua`, and `SENTINEL` must still be nil.
local kernel = require("embedded.knl")

print("EMBEDDED_KNL_TYPE=" .. type(kernel))
print("HAS_BEAT=" .. type(kernel.beat))
print("SENTINEL=" .. tostring(kernel.sentinel))
