-- prompt_flag.lua — verify _PROMPT and _CONTEXT globals injected by host
if _PROMPT then
    print("PROMPT:" .. _PROMPT)
else
    print("PROMPT:nil")
end
if _CONTEXT then
    print("CONTEXT:" .. _CONTEXT)
else
    print("CONTEXT:nil")
end
