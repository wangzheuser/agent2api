//! 经真实转发入口保护空池等待和取消，而不只测试适配器中未被调用的方法。
use agent2api_server::server::core::providers::zcode::{
    captcha, credentials::ZcodeCredentials, region::Region,
};
use agent2api_server::server::core::upstream::{
    cancellation, usage::RequestTelemetry, ForwardOutcome, ForwardRequest, UpstreamService,
};
use agent2api_server::server::core::{account_store::AccountStore, auth::AuthService};
use agent2api_server::server::{config, db::Db};
use axum::{
    http::{HeaderMap, StatusCode},
    routing::post,
    Router,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"fixture\",\"model\":\"glm-5.3-flash\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

#[tokio::test]
async fn both_regions_wait_for_refill_and_cancel_without_sending_empty_proofs() {
    // 独立集成测试进程；数据库和两地区上游都仅指向本地构造对象。
    let dir = std::env::temp_dir().join(format!(
        "zcode-restore-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    let db = Db::open(&dir.join("fixture.db")).unwrap();
    config::init(Some(db.clone()));
    let store = AccountStore::with_db(Some(db));
    let service = UpstreamService::new(store.clone(), AuthService::for_store(store.clone()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let received = seen.clone();
    let app = Router::new().route(
        "/anthropic/v1/messages",
        post(move |headers: HeaderMap| {
            received.lock().unwrap().push(
                headers
                    .get("x-aliyun-captcha-verify-param")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string(),
            );
            async { (StatusCode::OK, [("content-type", "text/event-stream")], SSE) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    for region in Region::ALL {
        std::env::set_var(format!("{}PLAN_BASE_URL", region.env_prefix()), &base);
        let credentials = ZcodeCredentials {
            region,
            access_token: "fixture-only".into(),
            jwt: "fixture-only".into(),
            user_id: region.provider_id().into(),
            device_mid: "fixture-device".into(),
        };
        store
            .add_zcode_account(&credentials, None, "fixture")
            .unwrap();
        let id = credentials.account_id();
        store.update_zcode_plan(&id, &json!("start-plan")).unwrap();
        let request = |telemetry| ForwardRequest {
            body: json!({"model":"glm-5.3-flash","messages":[{"role":"user","content":"fixture"}]}),
            stream: false,
            dedupe_key: String::new(),
            client_headers: HeaderMap::new(),
            telemetry,
            allowed_providers: None,
            pinned_account: Some(id.clone()),
        };
        assert_eq!(captcha::ready(), 0);
        let proof = format!("fixture-proof-{}", region.provider_id());
        let refill = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                captcha::stats()["waiting"],
                1,
                "转发应等待补货而非立即返回 503"
            );
            captcha::push(&proof, "fixture");
        };
        let (result, ()) = tokio::join!(
            service.forward(request(Arc::new(RequestTelemetry::new()))),
            refill
        );
        match result.unwrap() {
            ForwardOutcome::Completion { body } => {
                assert_eq!(body["choices"][0]["message"]["content"], "OK")
            }
            _ => panic!("应聚合本地上游的真实 SSE"),
        }
        assert_eq!(seen.lock().unwrap().last(), Some(&proof));
        assert_eq!(captcha::ready(), 0);
        let telemetry = Arc::new(RequestTelemetry::new());
        let token = cancellation::register(&id).unwrap();
        telemetry.set_cancel_token(token.clone());
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(captcha::stats()["waiting"], 1);
            token.cancel();
        };
        let (result, ()) = tokio::join!(service.forward(request(telemetry)), cancel);
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("已取消请求不应发送"),
        };
        assert_eq!(error.status_code, 408);
        assert_eq!(captcha::stats()["waiting"], 0);
        cancellation::unregister(&id);
    }
    assert_eq!(seen.lock().unwrap().len(), 2);
    server.abort();
}
