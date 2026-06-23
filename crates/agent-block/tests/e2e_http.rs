mod common;

use predicates::prelude::*;

#[test]
fn http_request_connection_error() {
    // Without HTTP_TEST_URL, the fixture tries to connect to 127.0.0.1:1
    // which should fail with a connection error.  This verifies that
    // http.request exists and returns errors properly.
    common::agent_block_cmd()
        .args(["-s", &common::fixture("http_get.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("error_ok"));
}
