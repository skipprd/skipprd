fn normalize_spec(src: &str) -> String {
    src.replace("\r\n", "\n").trim().to_string()
}

#[test]
fn ask_ws_spec_matches_runtime_core_spec() {
    let ask_ws = normalize_spec(include_str!("../../../ask-ws.yaml"));
    let ws_core = normalize_spec(include_str!("../openapi/ws-core.yaml"));
    assert_eq!(
        ask_ws, ws_core,
        "ask-ws.yaml must stay byte-equivalent to runtime/openapi/ws-core.yaml"
    );
}

#[test]
fn runtime_core_spec_uses_plans_array_contract() {
    let ws_core = include_str!("../openapi/ws-core.yaml");
    assert!(
        ws_core.contains("required: [v, type, server_time, seq, thread_id, plans]"),
        "PlansResponse must require plans[] in hard-cutover schema"
    );
    assert!(
        ws_core.contains("changed_plan_keys:"),
        "PlansChangedResponse must expose changed_plan_keys"
    );
}
