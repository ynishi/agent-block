-- Echo only the variables under test, never the whole environment: this fixture
-- runs in CI, and assert_cmd prints captured stdout verbatim when an assertion
-- fails, so an `env` dump would put every real CI secret into the log.
local cmd = table.concat({
    'echo "A=$ANTHROPIC_API_KEY"',
    'echo "O=$OPENAI_API_KEY"',
    'echo "M=$AGENT_BLOCK_MESH_SECRET_KEY"',
    'echo "V=$TEST_VISIBLE_VAR"',
}, "; ")

local result = sh.exec(cmd)
print("ok=" .. tostring(result.ok))
print(result.stdout)
