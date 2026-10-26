use agent2api_server::server::core::key_scope::KeyScope;
use agent2api_server::server::core::providers::adapter::{
    adapter_for, SuccessHead, UpstreamErrorClass, UpstreamStopReason,
};
use agent2api_server::server::core::providers::ProviderKind;
use agent2api_server::server::core::upstream::{
    usage::RequestTelemetry, ForwardOutcome, ForwardRequest,
};
use agent2api_server::server::core::workbuddy_safety;
use agent2api_server::server::core::{
    account_store::AccountStore, auth::AuthService, upstream::UpstreamService,
};
use agent2api_server::server::{config, logging};
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn request(provider: &str, pinned: Option<&str>, stream: bool) -> ForwardRequest {
    ForwardRequest {
        body: json!({"model":"fixture-risk-model","messages":[{"role":"user","content":"fixture"}],"stream":stream}),
        stream,
        dedupe_key: String::new(),
        client_headers: HeaderMap::new(),
        telemetry: Arc::new(RequestTelemetry::with_id()),
        allowed_providers: Some(KeyScope::provider_only(provider)),
        pinned_account: pinned.map(str::to_string),
        route_session: None,
        ignore_model_gate: true,
    }
}

#[test]
fn risk_classification_preserves_codes_and_separates_capacity() {
    for kind in [ProviderKind::WorkBuddy, ProviderKind::WorkBuddyIntl] {
        let adapter = adapter_for(kind);
        for (status, body, expected) in [
            (
                403,
                json!({"code":11140,"msg":"request illegal","displayMsg":{"en":"Your account has been restricted due to unusual activity"}}),
                UpstreamStopReason::AccountRestricted,
            ),
            (
                200,
                json!({"code":"11140","msg":"request illegal"}),
                UpstreamStopReason::AccountRestricted,
            ),
            (
                400,
                json!({"code":"11-128","msg":"unapproved channel"}),
                UpstreamStopReason::ChannelRefused,
            ),
            (
                400,
                json!({"code":11128,"msg":"unapproved channel"}),
                UpstreamStopReason::ChannelRefused,
            ),
            (
                403,
                json!({"code":"11-128","msg":"request illegal"}),
                UpstreamStopReason::ChannelRefused,
            ),
            (
                403,
                json!({"code":11140,"msg":"request illegal","displayMsg":{"en":"Illegal API invocation from an unapproved channel"}}),
                UpstreamStopReason::ChannelRefused,
            ),
            (
                429,
                json!({"code":6021,"msg":"queue.full"}),
                UpstreamStopReason::UpstreamBusy,
            ),
            // 实测真实响应（2026-10-26 16:06）：排队等待也是「队列繁忙」，
            // 不是账号额度耗尽 —— 判成 QuotaLimited 会给账号打上冷却。
            (
                429,
                json!({"code":6020,"msg":"queue.waiting.title","requestId":"fixture","data":{"queue_position":4028,"queue_size":"999+","retry_after":40,"estimated_wait":20140,"client_user_type":"free"}}),
                UpstreamStopReason::UpstreamBusy,
            ),
        ] {
            assert!(
                matches!(adapter.classify_error(status, &body), UpstreamErrorClass::Terminal { kind, .. } if kind == expected),
                "status={status} body={body} expected={expected:?}"
            );
            assert!(matches!(
                adapter.inspect_success_head(format!("data: {body}\n\n").as_bytes()),
                SuccessHead::Failure(_)
            ));
        }
        assert!(!matches!(
            adapter.classify_error(
                403,
                &json!({"code":11140,"msg":"request illegal: rate limiting"})
            ),
            UpstreamErrorClass::Terminal {
                kind: UpstreamStopReason::AccountRestricted,
                ..
            }
        ));
        assert!(matches!(
            adapter.classify_error(
                403,
                &json!({"msg":"corporate account has been restricted"})
            ),
            UpstreamErrorClass::Terminal {
                kind: UpstreamStopReason::AccountRestricted,
                ..
            }
        ));
        for message in [
            "rate_limited",
            "rate-limited",
            "rate limiting",
            "RATE_LIMITING",
        ] {
            assert!(!matches!(
                adapter.classify_error(
                    403,
                    &json!({"code":11140,"msg":format!("request illegal: {message}")})
                ),
                UpstreamErrorClass::Terminal {
                    kind: UpstreamStopReason::AccountRestricted,
                    ..
                }
            ));
        }
        for body in [
            json!({"msg":"request illegal"}),
            json!({"code":99999,"msg":"request illegal"}),
        ] {
            assert!(matches!(
                adapter.classify_error(403, &body),
                UpstreamErrorClass::Terminal {
                    kind: UpstreamStopReason::ChannelRefused,
                    ..
                }
            ));
        }
        let role = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n";
        assert!(matches!(
            adapter.inspect_success_head(role.as_bytes()),
            SuccessHead::Pending
        ));
        let blocked = format!("{role}data: {{\"code\":11140,\"msg\":\"request illegal\"}}\n\n");
        assert!(matches!(
            adapter.inspect_success_head(blocked.as_bytes()),
            SuccessHead::Failure(UpstreamErrorClass::Terminal {
                kind: UpstreamStopReason::AccountRestricted,
                ..
            })
        ));
        assert!(matches!(
            adapter.classify_error(400, &json!({"msg":"Illegal API invocation"})),
            UpstreamErrorClass::Terminal {
                kind: UpstreamStopReason::ChannelRefused,
                ..
            }
        ));
        assert!(matches!(
            adapter.classify_error(429, &json!({"code":6004,"msg":"quota"})),
            UpstreamErrorClass::QuotaLimited { .. }
        ));
        assert!(matches!(
            adapter.inspect_success_head(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n"
            ),
            SuccessHead::Ready
        ));
    }
}

/// 排队（6020/`queue.waiting`）与额度耗尽（6004）必须是两条路径：
/// 前者不罚账号，后者的冷却时长要尽量取上游给的恢复时间。
///
/// 背景（实测事故 2026-10-26 16:06）：上游对免费用户排队，原始响应是
/// `{"code":6020,"msg":"queue.waiting.title","data":{"retry_after":40,...}}`。
/// 当时 6020 掉进了 `QuotaLimited`，文案里又解析不出恢复时间，于是
/// `mark_rate_limited` 用兜底值把整个账号池锁了 10 分钟 —— 上游说 40 秒，
/// 本地挂 10 分钟，后续请求全在选路阶段被秒拒。
#[test]
fn queue_waiting_stays_queued_and_retry_after_shortens_cooldown() {
    for kind in [ProviderKind::WorkBuddy, ProviderKind::WorkBuddyIntl] {
        let adapter = adapter_for(kind);
        // ① 6020 + queue.waiting → 排队，不是限额
        let queued = json!({
            "code": 6020,
            "msg": "queue.waiting.title",
            "requestId": "fixture",
            "data": {"queue_position": 4028, "queue_size": "999+", "retry_after": 40, "estimated_wait": 20140, "client_user_type": "free"},
        });
        assert!(
            matches!(
                adapter.classify_error(429, &queued),
                UpstreamErrorClass::Terminal { kind: UpstreamStopReason::UpstreamBusy, status: 503, .. }
            ),
            "6020 queue.waiting must stay queued: {kind:?}"
        );

        // ①' `code` 被上游换掉时，文案是唯一信号（仍限定 HTTP 429 ——
        //     200 正文块里出现「排队」字样不能当成业务错误）
        assert!(
            matches!(
                adapter.classify_error(429, &json!({"code": 7001, "msg": "queue.waiting.title"})),
                UpstreamErrorClass::Terminal { kind: UpstreamStopReason::UpstreamBusy, .. }
            ),
            "queue text alone must still stop as busy: {kind:?}"
        );
        assert!(
            matches!(
                adapter.inspect_success_head(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"请排队等待\"}}]}\n\n".as_bytes()
                ),
                SuccessHead::Ready
            ),
            "ordinary content must not be read as a queue error: {kind:?}"
        );

        // ② 真限额（6004）带结构化 retry_after → 用它当恢复时间，
        //    不再落到「缺失即 10 分钟」的兜底
        let before = logging::now_ms();
        let quota = json!({"code": 6004, "msg": "too many requests", "data": {"retry_after": 40}});
        let UpstreamErrorClass::QuotaLimited { reset_at, .. } = adapter.classify_error(429, &quota) else {
            panic!("6004 must stay QuotaLimited: {kind:?}");
        };
        let reset_at = reset_at.expect("retry_after must yield a reset time");
        assert!(
            (reset_at - before - 40_000).abs() <= 5_000,
            "retry_after=40s should become ~40s cooldown, got {}ms",
            reset_at - before
        );

        // ③ 没有结构化建议值时才走兜底（交给 mark_rate_limited），这里保持 None
        assert!(matches!(
            adapter.classify_error(429, &json!({"code": 6004, "msg": "quota"})),
            UpstreamErrorClass::QuotaLimited { reset_at: None, .. }
        ));

        // ④ 异常建议值不放大冷却：超上限截断到 10 分钟、过小视为没给
        let huge = json!({"code": 6004, "msg": "quota", "data": {"retry_after": 86400}});
        let UpstreamErrorClass::QuotaLimited { reset_at: Some(reset_at), .. } =
            adapter.classify_error(429, &huge)
        else {
            panic!("huge retry_after must still be QuotaLimited: {kind:?}");
        };
        assert!(
            reset_at - logging::now_ms() <= 600_000,
            "retry_after must be capped at the 10min fallback"
        );
        assert!(matches!(
            adapter.classify_error(429, &json!({"code": 6004, "msg": "quota", "data": {"retry_after": 0}})),
            UpstreamErrorClass::QuotaLimited { reset_at: None, .. }
        ));
    }
}

#[tokio::test]
async fn terminals_stop_replay_and_rotation_and_limits_release_on_drop() {    let dir = std::env::temp_dir().join(format!(
        "workbuddy-risk-{}-{}",
        std::process::id(),
        logging::now_ms()
    ));
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = agent2api_server::server::db::Db::open(&dir.join("agent2api.db")).unwrap();
    config::init(Some(db.clone()));
    agent2api_server::server::core::task_state::install(Some(db.clone()));
    agent2api_server::server::core::usage_records::install(Some(db.clone()));
    agent2api_server::server::core::limiter::install(Some(db.clone()));
    let store = AccountStore::with_db(Some(db.clone()));
    let auth = AuthService::new(
        store.clone(),
        agent2api_server::server::core::endpoints::default_context(),
    );
    let upstream = UpstreamService::new(store.clone(), auth);
    assert!(config::update_raw_field("retryCount", json!(2)));
    assert!(config::update_raw_field(
        "retryCrossProviderCount",
        json!(2)
    ));
    assert!(config::update_raw_field("retryIntervalSeconds", json!(0)));
    assert!(config::update_raw_field(
        "noRetryStatusCodes",
        json!([403, 429, 503])
    ));
    let mode = Arc::new(Mutex::new((
        403u16,
        json!({"code":11140,"msg":"request illegal"}),
        false,
    )));
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let incoming = seen.clone();
    let response_mode = mode.clone();
    let app = Router::new().fallback_service(post(move |headers: HeaderMap| {
        let incoming = incoming.clone();
        let mode = response_mode.clone();
        async move {
            incoming.lock().unwrap().push(
                headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .into(),
            );
            let (status, payload, streaming) = mode.lock().unwrap().clone();
            if streaming {
                let first = futures::stream::once(async {
                    Ok::<_, std::io::Error>(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"fixture\"}}]}\n\n",
                    )
                });
                let tail = futures::stream::pending::<Result<&str, std::io::Error>>();
                return (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(first.chain(tail)),
                )
                    .into_response();
            }
            if payload["fixtureSse"] == true {
                return (
                    [("content-type", "text/event-stream")],
                    format!("data: {payload}\n\n"),
                )
                    .into_response();
            }
            (StatusCode::from_u16(status).unwrap(), Json(payload)).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    for provider in ["workbuddy", "workbuddy-intl"] {
        for (index, (status, body, paused)) in [
            (403, json!({"code":11140,"msg":"request illegal"}), true),
            (403, json!({"msg":"request illegal"}), false),
            (403, json!({"code":99999,"msg":"request illegal"}), false),
            (
                400,
                json!({"code":"11-128","msg":"unapproved channel"}),
                false,
            ),
            (400, json!({"code":11128,"msg":"unapproved channel"}), false),
            (429, json!({"code":6021,"msg":"queue.full"}), false),
            // 6020 排队：一次发送即停（不换号放大），且**不得**写限额冷却
            (
                429,
                json!({"code":6020,"msg":"queue.waiting.title","data":{"queue_position":4028,"queue_size":"999+","retry_after":40,"estimated_wait":20140}}),
                false,
            ),
            (200, json!({"code":11140,"msg":"request illegal"}), true),
            (
                200,
                json!({"code":"11-128","msg":"unapproved channel","fixtureSse":true}),
                false,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let mut ids = Vec::new();
            for side in ["a", "b"] {
                let uid = format!("{provider}-{index}-{side}");
                let account = store.add_account(&json!({"account":{"uid":uid},"auth":{"accessToken":uid},"endpoint":endpoint}), None, Some(provider)).unwrap();
                ids.push(account["id"].as_str().unwrap().to_string());
            }
            *mode.lock().unwrap() = (status, body, false);
            seen.lock().unwrap().clear();
            let result = upstream.forward(request(provider, None, false)).await;
            assert!(result.is_err(), "{provider}/{index}");
            let error = result.err().unwrap();
            assert_eq!(
                error.status_code,
                if status == 429 { 503 } else { 403 },
                "normalized terminal status: {provider}/{index}"
            );
            let sent = seen.lock().unwrap().clone();
            assert_eq!(
                sent.len(),
                1,
                "terminal must not replay or rotate: {sent:?}"
            );
            let sent_id = ids
                .iter()
                .find(|id| {
                    let session = store.get_session_by_id(id).unwrap().session;
                    sent[0]
                        == format!(
                            "Bearer {}",
                            session["auth"]["accessToken"].as_str().unwrap()
                        )
                })
                .unwrap();
            let first = store.get_session_by_id(sent_id).unwrap().session;
            assert_eq!(
                workbuddy_safety::status(&store, &first).unwrap()["paused"],
                paused
            );
            let accounts = store.accounts_for_provider(provider);
            for account in accounts
                .iter()
                .filter(|account| ids.iter().any(|id| account["id"] == *id))
            {
                assert!(
                    account["rateLimits"]
                        .as_object()
                        .is_none_or(|limits| limits.is_empty()),
                    "capacity/channel/restriction must not charge quota: {account}"
                );
            }
            if paused {
                let reopened = agent2api_server::server::db::Db::open(db.file()).unwrap();
                let reopened_store =
                    agent2api_server::server::core::account_store::AccountStore::with_db(Some(
                        reopened,
                    ));
                assert!(
                    workbuddy_safety::check(&reopened_store, &first).is_err(),
                    "pause persists across DB reopen"
                );
                assert!(upstream
                    .forward(request(provider, Some(sent_id), false))
                    .await
                    .is_err());
                assert_eq!(
                    seen.lock().unwrap().len(),
                    1,
                    "pinned request cannot bypass pause"
                );
            }
            for id in ids {
                store
                    .update_account(&id, &json!({"enabled":false}))
                    .unwrap();
            }
        }
        let account = store.add_account(&json!({"account":{"uid":format!("{provider}-cap")},"auth":{"accessToken":"fixture-cap"},"endpoint":endpoint}), None, Some(provider)).unwrap();
        let id = account["id"].as_str().unwrap();
        store
            .update_account(id, &json!({"maxConcurrent":1}))
            .unwrap();
        *mode.lock().unwrap() = (200, Value::Null, true);
        seen.lock().unwrap().clear();
        let (first, second) = tokio::join!(
            upstream.forward(request(provider, Some(id), true)),
            upstream.forward(request(provider, Some(id), true))
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "cap=1 must reserve atomically"
        );
        drop(first);
        drop(second);
        let next = upstream
            .forward(request(provider, Some(id), true))
            .await
            .unwrap();
        assert!(matches!(next, ForwardOutcome::Stream { .. }));
        drop(next);
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "dropped stream releases capacity"
        );
        store
            .update_account(id, &json!({"maxConcurrent":0}))
            .unwrap();
        let (first, second) = tokio::join!(
            upstream.forward(request(provider, Some(id), true)),
            upstream.forward(request(provider, Some(id), true))
        );
        assert!(
            first.is_ok() && second.is_ok(),
            "explicit zero keeps unlimited concurrency"
        );
        drop(first);
        drop(second);
        let before_cooldown = seen.lock().unwrap().len();
        store
            .mark_rate_limited(
                id,
                "fixture-risk-model",
                429,
                Some(6004),
                Some((logging::now_ms() + 60000) as f64),
                "fixture quota",
            )
            .unwrap();
        assert!(upstream
            .forward(request(provider, Some(id), false))
            .await
            .is_err());
        assert_eq!(
            seen.lock().unwrap().len(),
            before_cooldown,
            "pinned request cannot bypass cooldown"
        );
        store.update_account(id, &json!({"enabled":false})).unwrap();
    }
    server.abort();
}

#[test]
fn environment_identity_and_catalog_pause_are_isolated() {
    const CHILD: &str = "AGENT2API_RISK_ENV_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "environment_identity_and_catalog_pause_are_isolated",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env_remove("WORKBUDDY_EDITION")
            .env_remove("WORKBUDDY_TOKEN")
            .env_remove("WORKBUDDY_INTL_TOKEN")
            .status()
            .unwrap();
        assert!(result.success());
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        use agent2api_server::server::core::{models::ModelCatalog, providers::workbuddy::Region};
        let dir = std::env::temp_dir().join(format!("workbuddy-env-risk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("AGENT2API_PROXY_HOME", &dir);
        let db = agent2api_server::server::db::Db::open(&dir.join("fixture.db")).unwrap();
        config::init(Some(db.clone()));
        agent2api_server::server::core::task_state::install(Some(db.clone()));
        agent2api_server::server::core::usage_records::install(Some(db.clone()));
        agent2api_server::server::core::limiter::install(Some(db.clone()));
        let store = AccountStore::with_db(Some(db));
        let auth = AuthService::for_store(store.clone());
        let seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let calls = seen.clone();
        let app = Router::new().fallback(move |uri: axum::http::Uri, headers: HeaderMap| {
            let calls = calls.clone();
            async move {
                calls.lock().unwrap().push((
                    uri.path().to_owned(),
                    headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned(),
                ));
                Json(json!({"choices":[{"message":{"content":"fixture"},"finish_reason":"stop"}]}))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for (region, prefix) in [(Region::Cn, "WORKBUDDY"), (Region::Intl, "WORKBUDDY_INTL")] {
            let provider = region.provider_id();
            std::env::set_var(format!("{prefix}_TOKEN"), format!("fixture-{provider}"));
            std::env::set_var(format!("{prefix}_USER_ID"), format!("uid-{provider}"));
            std::env::set_var(format!("{prefix}_ENDPOINT"), &endpoint);
            let actual = auth
                .get_current_session_for(provider)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(actual["provider"], provider);
            assert_eq!(actual["endpoint"], endpoint);
            workbuddy_safety::pause(&store, &actual).unwrap();
            // 管理入口使用相同只读快照，不能退回无 UID 的默认槽位。
            let management = auth.default_session_snapshot_for(provider).unwrap();
            let status = workbuddy_safety::status(&store, &management).unwrap();
            assert_eq!(status["paused"], true);
            assert_eq!(seen.lock().unwrap().len(), 0);
            let outcome = ModelCatalog::new(region)
                .refresh_with_current_account(&store, &auth, "")
                .await;
            assert!(!outcome.refreshed);
            assert_eq!(
                seen.lock().unwrap().len(),
                0,
                "paused env catalog must not send"
            );
            workbuddy_safety::resume(
                &store,
                &management,
                status["identity"].as_str().unwrap(),
                status["revision"].as_u64().unwrap(),
            )
            .unwrap();
            workbuddy_safety::check(&store, &actual).unwrap();
            let upstream =
                UpstreamService::new(store.clone(), AuthService::for_store(store.clone()));
            let result = upstream.forward(request(provider, None, false)).await;
            assert!(result.is_ok(), "{provider} env forwarding failed");
            let calls = seen.lock().unwrap().clone();
            assert_eq!(
                calls,
                vec![(
                    "/v2/chat/completions".to_owned(),
                    format!("Bearer fixture-{provider}")
                )]
            );
            seen.lock().unwrap().clear();
            std::env::remove_var(format!("{prefix}_TOKEN"));
        }
        assert!(auth
            .default_session_snapshot_for("workbuddy-intl")
            .is_none());
        let account = store
            .add_account(
                &json!({"account":{"uid":"catalog-paused"},
            "auth":{"accessToken":"fixture","refreshToken":"fixture-refresh","expiresAt":1},
            "endpoint":endpoint}),
                None,
                Some("workbuddy"),
            )
            .unwrap();
        let id = account["id"].as_str().unwrap();
        let session = store.get_session_by_id(id).unwrap().session;
        workbuddy_safety::pause(&store, &session).unwrap();
        let catalog = ModelCatalog::new(Region::Cn);
        for selected in ["", id] {
            let outcome = catalog
                .refresh_with_current_account(&store, &auth, selected)
                .await;
            assert!(!outcome.refreshed);
            assert_eq!(
                seen.lock().unwrap().len(),
                0,
                "paused account must not refresh token or catalog"
            );
        }
        let intl = ModelCatalog::new(Region::Intl)
            .refresh_with_current_account(&store, &auth, id)
            .await;
        assert!(
            !intl.refreshed,
            "cross-region catalog must not borrow an account"
        );
        assert_eq!(seen.lock().unwrap().len(), 0);
        server.abort();
    });
}

#[test]
fn refresh_and_late_sse_restrictions_stop_without_replay() {
    const CHILD: &str = "AGENT2API_RISK_REFRESH_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "refresh_and_late_sse_restrictions_stop_without_replay",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env_remove("WORKBUDDY_TOKEN")
            .env_remove("WORKBUDDY_INTL_TOKEN")
            .status()
            .unwrap();
        assert!(result.success());
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let dir = std::env::temp_dir().join(format!("workbuddy-refresh-risk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("AGENT2API_PROXY_HOME", &dir);
        let db = agent2api_server::server::db::Db::open(&dir.join("fixture.db")).unwrap();
        config::init(Some(db.clone()));
        agent2api_server::server::core::task_state::install(Some(db.clone()));
        agent2api_server::server::core::usage_records::install(Some(db.clone()));
        agent2api_server::server::core::limiter::install(Some(db.clone()));
        let store = AccountStore::with_db(Some(db));
        let upstream = UpstreamService::new(store.clone(), AuthService::for_store(store.clone()));
        assert!(config::update_raw_field("retryCount", json!(2)));
        assert!(config::update_raw_field("retryCrossProviderCount", json!(2)));
        assert!(config::update_raw_field("retryIntervalSeconds", json!(0)));
        assert!(config::update_raw_field("noRetryStatusCodes", json!([403, 429, 503])));
        let seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let mode = Arc::new(Mutex::new(String::new()));
        let release = Arc::new(tokio::sync::Notify::new());
        let (calls, selected, unblock) = (seen.clone(), mode.clone(), release.clone());
        let app = Router::new().fallback(move |uri: axum::http::Uri, headers: HeaderMap| {
            let (calls, selected, unblock) = (calls.clone(), selected.clone(), unblock.clone());
            async move {
                let path = uri.path().to_owned();
                calls.lock().unwrap().push((path.clone(), headers.get("authorization")
                    .and_then(|v| v.to_str().ok()).unwrap_or("").to_owned()));
                if path.ends_with("/auth/token/refresh") {
                    return (StatusCode::FORBIDDEN, Json(json!({"code":11140,"msg":"request illegal"}))).into_response();
                }
                let mode = selected.lock().unwrap().clone();
                if mode == "after401" {
                    return (StatusCode::UNAUTHORIZED, Json(json!({"msg":"expired"}))).into_response();
                }
                let role = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n";
                let text = "data: {\"choices\":[{\"delta\":{\"content\":\"fixture-timely\"}}]}\n\n";
                let error = "data: {\"code\":11140,\"msg\":\"request illegal\"}\n\n";
                if mode == "role_same" {
                    return ([("content-type", "text/event-stream")], format!("{role}{error}")).into_response();
                }
                let first = if mode == "late" { text } else { role };
                let head = futures::stream::once(async move { Ok::<_, std::io::Error>(first.to_owned()) });
                let tail = futures::stream::once(async move {
                    if mode == "late" { unblock.notified().await; }
                    else { tokio::time::sleep(std::time::Duration::from_millis(30)).await; }
                    Ok::<_, std::io::Error>(error.to_owned())
                });
                ([("content-type", "text/event-stream")], Body::from_stream(head.chain(tail))).into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for provider in ["workbuddy", "workbuddy-intl"] {
            for case in ["proactive", "after401", "role_same", "role_split", "late"] {
                *mode.lock().unwrap() = case.to_owned();
                seen.lock().unwrap().clear();
                let mut ids = Vec::new();
                for side in ["a", "b"] {
                    let uid = format!("{provider}-{case}-{side}");
                    let expiry = if case == "proactive" { 1 } else { logging::now_ms() + 3_600_000 };
                    let account = store.add_account(&json!({"account":{"uid":uid},
                        "auth":{"accessToken":uid,"refreshToken":"fixture-refresh","expiresAt":expiry},
                        "endpoint":endpoint}), None, Some(provider)).unwrap();
                    ids.push(account["id"].as_str().unwrap().to_owned());
                }
                let result = tokio::time::timeout(std::time::Duration::from_secs(3),
                    upstream.forward(request(provider, None, true))).await.expect("first response must not wait for entire stream");
                if case == "late" {
                    let ForwardOutcome::Stream { mut stream, .. } = result.expect("body must be timely") else { panic!("expected stream") };
                    let first = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next()).await.unwrap().unwrap().unwrap();
                    assert!(String::from_utf8_lossy(&first).contains("fixture-timely"));
                    release.notify_one();
                    let mut output = String::new();
                    tokio::time::timeout(std::time::Duration::from_secs(2), async {
                        while let Some(item) = stream.next().await {
                            match item { Ok(bytes) => output.push_str(&String::from_utf8_lossy(&bytes)), Err(_) => break }
                        }
                    }).await.unwrap();
                    assert!(output.contains("error"), "late rejection must not become successful completion: {output}");
                } else {
                    assert!(result.is_err(), "{provider}/{case} must stop before returning success");
                }
                let calls = seen.lock().unwrap().clone();
                let chat_count = calls.iter().filter(|(path, _)| path.ends_with("/chat/completions")).count();
                let refresh_count = calls.iter().filter(|(path, _)| path.ends_with("/auth/token/refresh")).count();
                assert_eq!(chat_count, usize::from(case != "proactive"), "no chat replay/rotation: {calls:?}");
                assert_eq!(refresh_count, usize::from(matches!(case, "proactive" | "after401")), "no refresh replay: {calls:?}");
                let paused: Vec<_> = ids.iter().filter(|id| {
                    let session = store.get_session_by_id(id).unwrap().session;
                    workbuddy_safety::status(&store, &session).unwrap()["paused"] == true
                }).collect();
                assert_eq!(paused.len(), 1, "only the actual restricted account pauses");
                let before = calls.len();
                assert!(upstream.forward(request(provider, Some(paused[0]), false)).await.is_err());
                assert_eq!(seen.lock().unwrap().len(), before, "next request must stop before any upstream call");
                for id in ids { store.update_account(&id, &json!({"enabled":false})).unwrap(); }
            }
        }
        server.abort();
    });
}

/// 全池耗尽：候选账号都在同一模型的冷却里时，下游拿到的是 **503 + 短
/// `Retry-After`**，而不是上游那条带恢复时间的 429。
///
/// 为什么这是一条契约而不是细节：429 + `reset_at` 的形状只能表达「这个模型到
/// 某个时刻为止都没额度」。下游网关（magpie）照它把通道写成
/// `rest:{why:"quota",by:"resets",until:<上游的恢复时间>}` 并冻结 —— 本机账号
/// 十几分钟前就恢复了，对端也不会再来问。实测一条通道被冻了 17 小时
/// （`~/.config/magpie/routing/*.jsonl` 里 `try_ms` 从 1296 掉到 2）。
///
/// 同一处还要钉住：**钉住账号**（模型测试 / 指定账号）仍然透传 429 —— 那时下游
/// 问的就是「这一个账号行不行」，429 是准确答案，换成 503 反而在撒谎。
#[tokio::test]
async fn exhausted_pool_answers_503_with_a_short_retry_after() {
    let dir = std::env::temp_dir().join(format!(
        "workbuddy-pool-exhausted-{}-{}",
        std::process::id(),
        logging::now_ms()
    ));
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = agent2api_server::server::db::Db::open(&dir.join("agent2api.db")).unwrap();
    config::init(Some(db.clone()));
    agent2api_server::server::core::task_state::install(Some(db.clone()));
    agent2api_server::server::core::usage_records::install(Some(db.clone()));
    agent2api_server::server::core::limiter::install(Some(db.clone()));
    let store = AccountStore::with_db(Some(db.clone()));
    let auth = AuthService::new(
        store.clone(),
        agent2api_server::server::core::endpoints::default_context(),
    );
    let upstream = UpstreamService::new(store.clone(), auth);

    // 上游恒定：这条账号对这个模型已限额，恢复时间在**十几小时后**
    let hits = Arc::new(Mutex::new(0usize));
    let counter = hits.clone();
    let app = Router::new().fallback_service(post(move || {
        let counter = counter.clone();
        async move {
            *counter.lock().unwrap() += 1;
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "code": 6004,
                    "msg": "您的使用量已超出频率限制，将在 2026-10-27 15:38:27 UTC+8 重置，您也可以切换其他模型继续使用。"
                })),
            )
                .into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let provider = "workbuddy";
    let mut ids = Vec::new();
    for side in ["a", "b", "c"] {
        let uid = format!("{provider}-exhausted-{side}");
        let account = store
            .add_account(
                &json!({"account":{"uid":uid},"auth":{"accessToken":uid},"endpoint":endpoint}),
                None,
                Some(provider),
            )
            .unwrap();
        ids.push(account["id"].as_str().unwrap().to_string());
    }

    // ① 钉住账号：仍是上游那条 429（下游问的是「这一个账号行不行」）
    let pinned = upstream
        .forward(request(provider, Some(&ids[0]), false))
        .await
        .err()
        .expect("指定账号撞限额时必须失败");
    assert_eq!(pinned.status_code, 429, "钉住账号不该被改写成 503");
    assert!(pinned.message.contains("指定账号"), "{}", pinned.message);
    assert!(pinned.retry_after_secs.is_none(), "钉住账号不挂 Retry-After");
    assert_eq!(*hits.lock().unwrap(), 1);

    // ② 放开选路：两个还冷却着的账号都被试过之后，池子空了
    let error = upstream
        .forward(request(provider, None, false))
        .await
        .err()
        .expect("全池耗尽时必须失败");
    assert_eq!(
        *hits.lock().unwrap(),
        3,
        "剩下的两个账号应各试一次（换号本身没坏，坏的是最后的回包形状）"
    );
    assert_eq!(error.status_code, 503, "全池耗尽对下游是 503");
    assert_eq!(error.retry_after_secs, Some(60), "给一个短窗口让对端自己回来");
    assert_eq!(error.code.as_deref(), Some("no_available_account"));
    assert!(!error.is_quota_limit(), "503 不该再被当成账号限额");
    let payload = error.payload();
    assert!(
        payload["error"].get("reset_at").is_none(),
        "带 reset_at 就会被下游当成「到那时为止别再问」"
    );
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(
        !message.contains("UTC+8"),
        "上游的恢复时间只该进日志，不该进回包: {message}"
    );

    server.abort();
}
