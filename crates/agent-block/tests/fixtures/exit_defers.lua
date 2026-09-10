-- Looks at what it needs, does not find it, and says so before starting.
local job = require("job")
job.defer("no pod (nothing is listening)")
error("unreachable: defer ends the process")
