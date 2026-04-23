local lshape = require("lshape")
local T = lshape.t
local check = lshape.check

local User = T.shape({
    name = T.string,
    age = T.number,
})

local ok, why = check.check({ name = "Ada", age = 36 }, User)
if not ok then
    error("unexpected validation fail: " .. tostring(why))
end

local cats = lshape.luacats.gen({ User = User }, "AB")
if type(cats) ~= "string" or cats == "" then
    error("luacats.gen returned empty string")
end

print("lshape_ok")
print("luacats_ok")
