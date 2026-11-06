use super::*;

fn call(request: Value) -> Value {
    let output = Kernel.handle(&serde_json::to_vec(&request).unwrap());
    serde_json::from_slice(&output).unwrap()
}

#[test]
fn malformed_frames_and_payloads_return_static_codes() {
    let output = Kernel.handle(b"private-synthetic-sentinel");
    assert!(!String::from_utf8_lossy(&output).contains("private-synthetic-sentinel"));
    let response: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(response["error"]["code"], "invalid_request");
    for request in [json!([]), json!(["metera.kernel.v1", "describe", {}])] {
        assert_eq!(call(request)["error"]["code"], "invalid_request");
    }
    for payload in [Value::Null, json!([]), json!("synthetic")] {
        assert_eq!(
            call(json!({
                "protocol": PROTOCOL, "operation": "describe", "payload": payload
            }))["error"]["code"],
            "invalid_payload"
        );
    }
    assert_eq!(
        call(json!({"protocol":"other", "operation":"describe", "payload":{}}))["error"]["code"],
        "protocol_mismatch"
    );
    assert_eq!(
        call(json!({"protocol":PROTOCOL, "operation":"unsupported", "payload":{}}))["error"]["code"],
        "unsupported_operation"
    );
}

#[test]
fn description_names_only_the_implemented_pure_operations() {
    let response = call(json!({
        "protocol":PROTOCOL, "operation":"describe", "payload":{}
    }));
    assert_eq!(response["protocol"], PROTOCOL);
    assert_eq!(response["result"]["abi"], 1);
    assert_eq!(
        response["result"]["operations"],
        json!([
            "describe",
            "store.schema",
            "store.plan",
            "store.decode",
            "store.normalize",
            "store.advance"
        ])
    );
}

#[test]
fn an_empty_advance_batch_is_a_valid_empty_result() {
    let response = call(json!({
        "protocol":PROTOCOL, "operation":"store.advance", "payload":{"series":[]}
    }));
    assert_eq!(response["result"], json!({"advances":[]}));
}
