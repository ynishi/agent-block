-- Requires a module named `mylib` and reports where it came from.
--
-- Driven by tests/e2e_block_dirs.rs, which places `mylib` in the project
-- `lib/`, the user `lib/`, or (to prove it is NOT found there) `blocks/`.
local ok, mylib = pcall(require, "mylib")
if ok then
    print("MYLIB_FROM=" .. tostring(mylib.from))
else
    print("MYLIB_MISSING")
end
