use serde_json::{Value, json};
use sylvops_core::session_state::SessionStatus;

fn completion() -> Value {
    json!({
        "lifecycle": "running",
        "activity": "completed",
        "attention": {
            "id": 7, "reason": "successful_completion",
            "acknowledgement": "pending", "occurred_at": 100,
        },
    })
}

#[test]
fn expanded_state_roundtrips_through_messagepack_without_collapsing_dimensions() {
    let value = completion();
    let status: SessionStatus = serde_json::from_value(value.clone()).unwrap();
    let bytes = rmp_serde::to_vec_named(&status).unwrap();
    let decoded: SessionStatus = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
}

#[test]
fn expanded_state_rejects_unknown_values_and_unbounded_identities() {
    for path in [
        vec!["lifecycle"],
        vec!["activity"],
        vec!["attention", "reason"],
        vec!["attention", "acknowledgement"],
    ] {
        let mut value = completion();
        let mut field = &mut value;
        for key in path {
            field = &mut field[key];
        }
        *field = json!("unknown");
        assert!(serde_json::from_value::<SessionStatus>(value).is_err());
    }
    for id in [0, u64::MAX] {
        let mut value = completion();
        value["attention"]["id"] = json!(id);
        assert!(serde_json::from_value::<SessionStatus>(value).is_err());
    }
}

#[test]
fn expanded_state_rejects_impossible_combinations() {
    for (field, replacement) in [
        ("lifecycle", json!("exited")),
        ("activity", json!("working")),
        ("attention", Value::Null),
    ] {
        let mut value = completion();
        value[field] = replacement;
        assert!(serde_json::from_value::<SessionStatus>(value).is_err());
    }
    let mut value = completion();
    value["attention"]["occurred_at"] = json!(-1);
    assert!(serde_json::from_value::<SessionStatus>(value).is_err());
}
