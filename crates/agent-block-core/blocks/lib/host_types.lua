-- host_types — the run-time half of host_types.d.tl.
--
-- A Teal module requires `host_types` for its record types, and the generated
-- Lua keeps that `require`. Types are nothing at run time, so this is what
-- the name answers: an empty table. The globals themselves (`std`, …) are
-- already in the VM, put there by the host before any module loads.
return {}
