//! 本地 HTTP 上游验证：管理保存 → 三协议入口 → 选路 → 实际发送字节。
use agent2api_server::server::{config, http, ServerState};
use agent2api_server::server::core::{account_transfer, custom_providers, model_rules};
use agent2api_server::server::core::providers::zcode::{captcha, credentials::ZcodeCredentials, region::Region};
use axum::{body::Body, extract::Json, http::Request, routing::post, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const CHAT: &str = "data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"OK\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n";
const MESSAGES: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"fixture\",\"model\":\"glm-5.3-flash\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
const RESPONSES: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"fixture\",\"model\":\"fixture\"}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"OK\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";

async fn request(app: &Router, path: &str, body: &Value) -> (u16, String) {
    let response = app.clone().oneshot(Request::builder().method("POST").uri(path)
        .header("x-api-key", "fixture-key").header("content-type", "application/json")
        .body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 4_000_000).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn input(protocol: &str, model: &str, stream: bool, serial: usize) -> Value {
    let text = format!("fixture-{serial}");
    match protocol {
        "responses" => json!({"model":model,"input":text,"stream":stream,"reasoning":{"effort":"medium","summary":"auto"},"max_output_tokens":48000}),
        "messages" => json!({"model":model,"messages":[{"role":"user","content":text}],"stream":stream,"max_tokens":48000,"thinking":{"type":"adaptive"},"output_config":{"effort":"medium"}}),
        _ => json!({"model":model,"messages":[{"role":"user","content":text}],"stream":stream,"max_tokens":48000,"reasoning_effort":"medium"}),
    }
}

#[tokio::test]
async fn management_and_three_protocol_wire_force_clear_and_legacy_preservation() {
    let dir = std::env::temp_dir().join(format!("reasoning-override-{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    std::env::set_var("AGENT2API_PROXY_API_KEY", "fixture-key");
    let state = ServerState::bootstrap(0, "127.0.0.1".parse().unwrap()).unwrap();
    assert!(config::set_api_key(Some("fixture-key".into())));
    let panel = http::panel_router(state.clone());
    let gateway = http::gateway_router(state.clone());
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let mut mock = Router::new();
    for (path, sse) in [("/chat/completions", CHAT), ("/responses", RESPONSES), ("/v1/messages", MESSAGES), ("/anthropic/v1/messages", MESSAGES)] {
        let captured = seen.clone();
        mock = mock.route(path, post(move |Json(body): Json<Value>| {
            captured.lock().unwrap().push(body);
            async move { ([("content-type", "text/event-stream")], sse) }
        }));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap(); });
    let paths = [("chat", "/v1/chat/completions"), ("responses", "/v1/responses"), ("messages", "/v1/messages")];
    let mut serial = 0;
    for region in Region::ALL {
        std::env::set_var(format!("{}PLAN_BASE_URL", region.env_prefix()), &base);
        let credentials = ZcodeCredentials { region, access_token:"fixture-only".into(), jwt:"fixture-only".into(), user_id:region.provider_id().into(), device_mid:"fixture-device".into() };
        state.store().add_zcode_account(&credentials, None, "fixture").unwrap();
        state.store().update_zcode_plan(&credentials.account_id(), &json!("start-plan")).unwrap();
        let alias = format!("fixture/{}", region.provider_id());
        let binding = json!({"alias":alias,"target":"glm-5.3-flash","provider":region.provider_id(),"reasoning":"max"});
        let (status, result) = request(&panel, "/api/models/mappings", &binding).await;
        assert_eq!(status, 200, "{result}");
        for forced in [None, Some("max"), Some("")] {
            if let Some(level) = forced {
                let mut patch = binding.clone(); patch["reasoningOverride"] = json!(level);
                let (status, result) = request(&panel, "/api/models/mappings", &patch).await;
                assert_eq!(status, 200, "{result}");
            }
            // 开关补丁不能清除默认或强制值；读侧和 wire 使用同一绑定。
            let (status, result) = request(&panel, "/api/models/mappings", &json!({"alias":alias,"target":"glm-5.3-flash","provider":region.provider_id(),"enabled":true})).await;
            assert_eq!(status, 200, "{result}");
            let rules = model_rules::current(); let stored = rules.binding(region.provider_id(), &alias, "glm-5.3-flash").unwrap();
            assert_eq!(stored.reasoning.as_deref(), Some("max"));
            assert_eq!(stored.reasoning_override.as_deref(), forced.filter(|v| !v.is_empty()));
            for (protocol, path) in paths {
                for stream in [false, true] {
                    serial += 1; captcha::push(&format!("fixture-proof-{serial}"), "fixture");
                    let (status, result) = request(&gateway, path, &input(protocol, &alias, stream, serial)).await;
                    assert_eq!(status, 200, "{protocol} {stream}: {result}"); assert!(result.contains("OK"), "{result}");
                    let wire = seen.lock().unwrap().last().unwrap().clone();
                    assert_eq!(wire["model"], "glm-5.3-flash");
                    let is_forced = forced == Some("max");
                    assert_eq!(wire["thinking"]["budget_tokens"], if is_forced {32000} else {16000}, "{region:?} {protocol}: {wire}");
                    assert_eq!(wire["output_config"]["effort"], if is_forced {"max"} else {"high"});
                    // 沿用 Start Plan 原有配对规则：正文额度再加本档思考预算。
                    assert_eq!(wire["max_tokens"], if is_forced {80000} else {64000});
                }
            }
        }
        for invalid in [json!(true), json!("unknown"), json!("off"), json!("a".repeat(33))] {
            let mut patch = binding.clone(); patch["reasoningOverride"] = invalid;
            assert_eq!(request(&panel, "/api/models/mappings", &patch).await.0, 400);
        }
    }
    for protocol in ["chat_completions", "responses", "anthropic"] {
        let provider = custom_providers::create(&json!({"name":format!("fixture-{protocol}"),"protocol":protocol,"baseUrl":base})).unwrap();
        let id = provider["id"].as_str().unwrap();
        state.store().add_custom_account(id, &json!({"noAuth":true}), None).unwrap();
        let alias = format!("fixture/{protocol}");
        custom_providers::set_models(id, json!([{"id":"fixture","reasoning":"low","reasoningOverride":"max"}]), json!([{"alias":alias,"target":"fixture","reasoning":"high","reasoningOverride":"max"}])).unwrap();
        // 老客户端整表更新未带新键，不删除任何绑定的强制配置。
        let updated = custom_providers::set_models(id, json!([{"id":"fixture","reasoning":"low"}]), json!([{"alias":alias,"target":"fixture","reasoning":"high"}])).unwrap();
        assert_eq!(updated["models"][0]["reasoningOverride"], "max");
        assert_eq!(updated["mappings"][0]["reasoningOverride"], "max");
        for (downstream, path) in paths {
            for stream in [false, true] {
                serial += 1;
                let (status, result) = request(&gateway, path, &input(downstream, &alias, stream, serial)).await;
                assert_eq!(status, 200, "{protocol} {downstream}: {result}"); assert!(result.contains("OK"), "{result}");
                let wire = seen.lock().unwrap().last().unwrap().clone();
                assert_eq!(wire["model"], "fixture");
                match protocol {
                    "responses" => assert_eq!(wire["reasoning"]["effort"], "max"),
                    "anthropic" => assert_eq!(wire["thinking"]["budget_tokens"], 32768),
                    _ => assert_eq!(wire["reasoning_effort"], "max"),
                }
            }
        }
        // 别名清空强制不继承 target 的 max；同名 legacy 映射显式清空也不能回退。
        custom_providers::set_models(id, json!([{"id":"fixture","reasoning":"low","reasoningOverride":"max"}]), json!([
            {"alias":alias,"target":"fixture","reasoning":"high","reasoningOverride":null},
            {"alias":"fixture","target":"fixture","reasoningOverride":""}
        ])).unwrap();
        assert_eq!(custom_providers::wire_model_for(id, &alias), ("fixture".into(), Some("high".into()), None));
        assert_eq!(custom_providers::wire_model_for(id, "fixture").2, None);
        let normalized = custom_providers::get(id).unwrap();
        assert_eq!(normalized["mappings"][1]["reasoningOverride"], "");
        assert!(custom_providers::set_models(id, json!([{"id":"fixture","reasoningOverride":"unknown"}]), json!([])).is_err());
    }
    // 同名显式清空和模型强制值在导出/导入后仍独立。
    let exported = account_transfer::export_accounts(&state.store());
    let imported = account_transfer::import_accounts(&state.store(), &exported).unwrap();
    assert_eq!(imported["failed"], 0);
    for item in custom_providers::list() {
        let id = item["id"].as_str().unwrap();
        assert_eq!(item["models"][0]["reasoningOverride"], "max");
        assert_eq!(custom_providers::wire_model_for(id, "fixture").2, None);
    }
    // 配置/导入绕过保存预检后的错误必须终止，不能跨家降回未强制的默认。
    assert!(config::update_raw_field(config::KEY_ACCOUNT_SELECTION, json!("priority")));
    let alias = "fixture/no-fallback";
    let mut ids = Vec::new();
    for name in ["first", "fallback"] {
        let provider = custom_providers::create(&json!({"name":name,"baseUrl":base,"protocol":"chat_completions"})).unwrap();
        let id = provider["id"].as_str().unwrap().to_string();
        custom_providers::set_models(&id, json!([{"id":"fixture"}]), json!([{"alias":alias,"target":"fixture"}])).unwrap();
        state.store().add_custom_account(&id, &json!({"noAuth":true}), None).unwrap();
        ids.push(id);
    }
    let mut providers = custom_providers::list();
    providers.iter_mut().find(|p| p["id"] == ids[0]).unwrap()["mappings"][0]["reasoningOverride"] = json!("unknown");
    assert!(config::update_raw_field(custom_providers::KEY_CUSTOM_PROVIDERS, json!(providers)));
    let before = seen.lock().unwrap().len();
    serial += 1;
    let (status, result) = request(&gateway, "/v1/chat/completions", &input("chat", alias, false, serial)).await;
    assert_eq!(status, 400, "{result}"); assert!(result.contains(model_rules::OVERRIDE_ERROR_CODE), "{result}");
    assert_eq!(seen.lock().unwrap().len(), before, "强制错误不能发送到备用提供商");
    // 账号地区的目录必须与最终计划同源（Global 支持该模型，CN 不支持）。
    use agent2api_server::server::core::providers::qoder::{credentials::Credentials, endpoints::Region as QoderRegion};
    let credentials = Credentials { region:QoderRegion::Cn, access_token:"fixture-only".into(), refresh_token:String::new(), expires_at:Some(i64::MAX), user_id:"fixture-cn".into(), email:String::new(), name:String::new(), machine_id:"fixture-machine".into() };
    state.store().add_qoder_account(&credentials, None, "fixture").unwrap();
    model_rules::add_mapping("fixture/qoder-cn", "DeepSeek-Flash", Some("qoder"), None, None, &[], Some(Some("max"))).unwrap();
    serial += 1;
    let (status, result) = request(&gateway, "/v1/chat/completions", &input("chat", "fixture/qoder-cn", false, serial)).await;
    assert_eq!(status, 400, "{result}"); assert!(result.contains(model_rules::OVERRIDE_ERROR_CODE), "{result}");
    assert_eq!(seen.lock().unwrap().len(), before);
    // 删除全局绑定中的一家，不改变另一家的默认/强制等级。
    model_rules::add_mapping("fixture/global", "glm-5.3-flash", None, Some(Some("medium")), None, &[], Some(Some("max"))).unwrap();
    let (rules, removed) = model_rules::remove_mapping("fixture/global", "glm-5.3-flash", Some("zcode"), &["zcode-intl".into()]);
    assert!(removed);
    let retained = rules.binding("zcode-intl", "fixture/global", "glm-5.3-flash").unwrap();
    assert_eq!(retained.reasoning.as_deref(), Some("medium")); assert_eq!(retained.reasoning_override.as_deref(), Some("max"));
    server.abort();
    println!("verified {serial} local requests: both ZCode regions + 3 custom upstream protocols × 3 ingress protocols × stream/nonstream; set/default/clear preserved");
}
