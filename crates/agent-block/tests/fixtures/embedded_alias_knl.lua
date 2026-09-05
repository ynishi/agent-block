-- embedded_alias_knl.lua — `embedded.<name>` exists for sealed modules too.
--
-- Reading the kernel is fine; replacing it is what the seal refuses. The
-- alias is also the assertion that `embedded.` resolves from memory only:
-- one of the tests puts a decoy at `<project>/blocks/embedded/knl.lua`, and
-- `SENTINEL` must still be nil.
local kernel = require("embedded.knl")

print("EMBEDDED_KNL_TYPE=" .. type(kernel))
print("HAS_BEAT=" .. type(kernel.beat))
print("SENTINEL=" .. tostring(kernel.sentinel))
