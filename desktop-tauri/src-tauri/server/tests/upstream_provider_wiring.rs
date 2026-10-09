use agent2api_server::server::core::providers::{self, adapter::adapter_for, kuku};
use serde_json::json;

#[test]
fn kuku_cookie_shapes_preserve_business_exchange_credentials() {
    for payload in [
        json!("BDUSS=fixture-bduss-long; STOKEN=fixture-stoken; PTOKEN=fixture-ptoken; gfprotpl=old"),
        json!({"BDUSS":"fixture-bduss-long","STOKEN":"fixture-stoken","PTOKEN":"fixture-ptoken"}),
        json!({"cookie":"BDUSS=fixture-bduss-long; STOKEN=fixture-stoken; PTOKEN=fixture-ptoken"}),
        json!([{ "name":"BDUSS", "value":"fixture-bduss-long" },{ "name":"STOKEN", "value":"fixture-stoken" },{ "name":"PTOKEN", "value":"fixture-ptoken" }]),
    ] {
        let credential = kuku::credentials::credentials_from_payload(&payload).unwrap();
        let cookie = credential.cookie_header();
        assert!(cookie.contains("PTOKEN=fixture-ptoken"));
        assert!(cookie.contains("STOKEN=fixture-stoken"));
        assert_eq!(cookie.matches("BDUSS=").count(),1);
        assert_eq!(cookie.matches("gfprotpl=genflowpro").count(),1);
        assert_ne!(credential.token_tail(),credential.bduss);
    }
    for invalid in [json!({}),json!("STOKEN=fixture-stoken"),json!({"BDUSS":"short"})] {
        assert!(kuku::credentials::credentials_from_payload(&invalid).is_err());
    }
}

#[test]
fn kuku_registration_selects_stateful_forwarding_without_losing_native_providers() {
    let kind = providers::kind_from_id("kuku").unwrap();
    let adapter = adapter_for(kind);
    assert!(adapter.is_stateful());
    assert!(!adapter.supports_refresh());
    let models = adapter.list_models();
    assert!(models.iter().any(|model|model["id"]=="gateway-glm-5.3-flash"));
    assert_eq!(kuku::models::resolve_model("gateway-glm-5.3-flash"),"gateway-glm-5.3-flash");
    for provider in ["minimax-code","lobsterai","qoder","workbuddy","workbuddy-intl","zcode"] {
        assert_eq!(providers::kind_id(providers::kind_from_id(provider).unwrap()),provider);
    }
}
