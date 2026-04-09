-- Execute a shell command and print the result.
local result = sh.exec("echo ok")
print("ok=" .. tostring(result.ok))
print("code=" .. tostring(result.code))
print("stdout=" .. result.stdout)
