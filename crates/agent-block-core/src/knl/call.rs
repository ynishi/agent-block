//! K2 — the charge a model response costs the budget.
//!
//! This is the half of the model call that needs no VM: the arithmetic
//! that turns a response's usage counters into the amount the budget is
//! charged.  Keeping it here — split from the [`crate::bridge::knl`]
//! adapter — leaves it unit-testable and unable to call back into the
//! shell.  [`super::Session::append`] applies it when a `model_response`
//! is recorded.

use serde_json::{Map, Value};

use super::projection::{whole, USAGE_COUNTERS};

/// What a `model_response`'s usage costs the budget: input + output +
/// thinking, summed.
///
/// The counters are provider-supplied, so a missing one is `0` and the
/// total is floored at `0` — a backend reporting a negative counter cannot
/// turn a charge into a refund.  [`super::Session::append`] charges this
/// when a `model_response` is recorded.
pub fn charge_of(usage: &Map<String, Value>) -> i64 {
    USAGE_COUNTERS
        .iter()
        .fold(0i64, |total, counter| {
            total.saturating_add(usage.get(*counter).map_or(0, whole))
        })
        .max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_charge_sums_the_three_counters_and_never_goes_negative() {
        let charge = |usage: Value| charge_of(usage.as_object().expect("usage is a table"));

        assert_eq!(
            charge(json!({ "input_tokens": 10, "output_tokens": 3, "thinking_tokens": 7 })),
            20
        );
        // Missing counters are zero, and unknown ones are not charged.
        assert_eq!(charge(json!({ "input_tokens": 5 })), 5);
        assert_eq!(charge(json!({})), 0);
        assert_eq!(charge(json!({ "cache_read_tokens": 900 })), 0);
        // A provider that reports nonsense cannot hand budget back.
        assert_eq!(charge(json!({ "input_tokens": -50 })), 0);
        assert_eq!(charge(json!({ "input_tokens": "many" })), 0);
        // Whole floats are counted (Lua numbers arrive as floats).
        assert_eq!(charge(json!({ "input_tokens": 4.0 })), 4);
    }
}
