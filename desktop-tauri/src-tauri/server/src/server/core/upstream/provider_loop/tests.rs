use super::*;
use crate::server::core::providers::zcode::adapter::{ZCODE_ADAPTER, ZCODE_INTL_ADAPTER};
use crate::server::core::upstream::usage::RequestTelemetry;
use axum::{extract::State, routing::post, Json, Router};
use futures::TryStreamExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn native_reward_providers_http_200_errors_reach_the_rotation_classifier() {
    let body =
        "event: error\ndata: {\"error\":{\"code\":40201,\"message\":\"quota exceeded\"}}\n\n";
    for kind in [ProviderKind::MiniMaxCode, ProviderKind::LobsterAI] {
        let (transport, hits, task) = mock_upstream(200, "text/event-stream", body).await;
        let failure = match send_with_retry(
            adapter_for(kind),
            &transport,
            &mut RetryBudget::new(3),
            None,
            &RequestTelemetry::new(),
            false,
            false,
            false,
        None,
    )
        .await
        {
            Ok(_) => panic!("首包业务错误不应进入成功下行"),
            Err(failure) => failure,
        };
        assert!(matches!(
            failure.class,
            UpstreamErrorClass::QuotaLimited { .. }
        ));
        assert_eq!(failure.error.status_code, 429);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        task.abort();
    }
}

#[tokio::test]
async fn success_head_wait_is_cancelled_without_waiting_for_another_chunk() {
    let app = Router::new().route(
        "/",
        post(|| async {
            let head = futures::stream::once(async {
                Ok::<_, std::io::Error>(Bytes::from_static(b"data: {"))
            });
            axum::body::Body::from_stream(head.chain(futures::stream::pending()))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let transport = TransportRequest {
        url: format!("http://{}/", listener.local_addr().unwrap()),
        headers: Vec::new(),
        payload: "{}".into(),
        proxy: None,
        system_proxy_when_unset: false,
    };
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let telemetry = RequestTelemetry::new();
    let token = cancellation::register("success-head-restore-fixture").unwrap();
    telemetry.set_cancel_token(token.clone());
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    };
    let run = async {
        let mut budget = RetryBudget::new(0);
        let (result, ()) = tokio::join!(
            send_with_retry(
                &ZCODE_ADAPTER,
                &transport,
                &mut budget,
                None,
                &telemetry,
                false,
                false
            , false, None),
            cancel
        );
        let failure = match result {
            Ok(_) => panic!("取消必须终止首包等待"),
            Err(failure) => failure,
        };
        assert_eq!(failure.error.status_code, 408);
        assert!(!failure.rebuild);
    };
    tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .unwrap();
    cancellation::unregister("success-head-restore-fixture");
    server.abort();
}

const QUOTA_BODY: &str = r#"{"code":1005,"msg":"exceed quota limit","logid":"test-request"}"#;

async fn mock_upstream(
    status: u16,
    content_type: &'static str,
    body: &'static str,
) -> (
    TransportRequest,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let app = axum::Router::new().route(
        "/messages",
        axum::routing::post(move || {
            count.fetch_add(1, Ordering::SeqCst);
            async move {
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    [(axum::http::header::CONTENT_TYPE, content_type)],
                    body,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/messages", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        TransportRequest {
            url,
            headers: vec![("Content-Type".into(), "application/json".into())],
            payload: r#"{"stream":true}"#.into(),
            proxy: None,
            // 测试替身按默认能力位（false = 直连），与生产里不带这一步的家同款
            system_proxy_when_unset: false,
        },
        hits,
        task,
    )
}

#[tokio::test]
async fn zcode_http_200_quota_json_is_classified_before_streaming_without_resend() {
    for adapter in [&ZCODE_ADAPTER, &ZCODE_INTL_ADAPTER] {
        let (transport, hits, task) =
            mock_upstream(200, "application/json; charset=utf-8", QUOTA_BODY).await;
        let mut budget = RetryBudget::new(3);
        let capture = Arc::new(crate::server::core::debug_traffic::TrafficCapture::begin(
            "quota-test",
        ));
        let result = send_with_retry(
            adapter,
            &transport,
            &mut budget,
            Some(&capture),
            &RequestTelemetry::new(),
            false,
            false,
            false,
        None,
    )
        .await;
        task.abort();
        let failure = match result {
            Err(failure) => failure,
            Ok(_) => panic!("HTTP 200 quota envelope must not enter the success stream"),
        };
        assert!(matches!(
            failure.class,
            UpstreamErrorClass::QuotaLimited {
                status: 429,
                upstream_code: Some(1005),
                reset_at: None,
                ..
            }
        ));
        assert_eq!(failure.error.status_code, 429);
        assert_eq!(failure.error.upstream_code, Some(1005));
        assert!(failure.error.message.contains("exceed quota limit"));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(budget.remaining, 3);
        assert_eq!(capture.captured_body(), QUOTA_BODY);
    }
}

#[test]
fn zcode_header_detection_handles_mime_parameters_without_requiring_sse_headers() {
    use axum::http::{header::CONTENT_TYPE, HeaderMap, HeaderValue};
    for (mime, expected) in [
        (None, false),
        (Some("application/json"), true),
        (Some("Application/JSON; charset=utf-8"), true),
        (Some("text/event-stream; charset=utf-8"), false),
        (Some("application/octet-stream"), false),
    ] {
        let mut headers = HeaderMap::new();
        if let Some(mime) = mime {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static(mime));
        }
        assert_eq!(ZCODE_ADAPTER.is_error_response(200, &headers), expected);
        assert!(ZCODE_ADAPTER.is_error_response(401, &headers));
        assert!(ZCODE_ADAPTER.is_error_response(429, &headers));
    }
}

#[tokio::test]
async fn zcode_http_auth_and_quota_errors_keep_their_status() {
    for (status, body) in [(401, ""), (429, r#"{"msg":"rate limited"}"#)] {
        let (transport, _, task) = mock_upstream(status, "application/json", body).await;
        let result = send_with_retry(
            &ZCODE_ADAPTER,
            &transport,
            &mut RetryBudget::new(0),
            None,
            &RequestTelemetry::new(),
            false,
            false,
            false,
        None,
    )
        .await;
        task.abort();
        let failure = match result {
            Err(failure) => failure,
            Ok(_) => panic!("HTTP error must not be accepted"),
        };
        assert_eq!(failure.error.status_code, i32::from(status));
        if status == 401 {
            assert!(matches!(
                failure.class,
                UpstreamErrorClass::TokenExpired { .. }
            ));
        } else {
            assert!(matches!(
                failure.class,
                UpstreamErrorClass::QuotaLimited { .. }
            ));
        }
    }
}

#[tokio::test]
async fn zcode_unknown_or_malformed_success_json_returns_an_error() {
    for body in [r#"{"code":9999,"msg":"unexpected rejection"}"#, "not json"] {
        let (transport, _, task) = mock_upstream(200, "application/json", body).await;
        let result = send_with_retry(
            &ZCODE_ADAPTER,
            &transport,
            &mut RetryBudget::new(0),
            None,
            &RequestTelemetry::new(),
            false,
            false,
            false,
        None,
    )
        .await;
        task.abort();
        let failure = match result {
            Err(failure) => failure,
            Ok(_) => panic!("non-SSE ZCode response must not be accepted"),
        };
        assert_eq!(failure.error.status_code, 502);
        assert!(failure.error.message.contains(if body == "not json" {
            "not json"
        } else {
            "unexpected rejection"
        }));
    }
}

#[tokio::test]
async fn zcode_success_sse_and_other_providers_json_remain_unconsumed() {
    let sse = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\"}}\n\n";
    let cases: [(&dyn ProviderAdapter, &str, &str); 2] = [
        (&ZCODE_ADAPTER, "text/event-stream", sse),
        (
            adapter_for(ProviderKind::WorkBuddy),
            "application/json",
            QUOTA_BODY,
        ),
    ];
    for (adapter, content_type, body) in cases {
        let (transport, _, task) = mock_upstream(200, content_type, body).await;
        let result = send_with_retry(
            adapter,
            &transport,
            &mut RetryBudget::new(0),
            None,
            &RequestTelemetry::new(),
            false,
            false,
            false,
        None,
    )
        .await;
        task.abort();
        let response = match result {
            Ok(response) => response,
            Err(failure) => panic!("valid response rejected: {}", failure.error.message),
        };
        let chunks = response.stream.try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(chunks.concat(), body.as_bytes());
    }
}

#[tokio::test]
async fn one_time_proof_retries_return_for_rebuild_while_normal_requests_reuse_transport() {
    use axum::{extract::State, http::HeaderMap, routing::post, Router};
    use std::sync::Mutex;
    for single_use in [true, false] {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let app = Router::new()
            .route(
                "/",
                post(
                    |State(seen): State<Arc<Mutex<Vec<String>>>>, headers: HeaderMap| async move {
                        let mut seen = seen.lock().unwrap();
                        seen.push(
                            headers
                                .get("x-proof")
                                .unwrap()
                                .to_str()
                                .unwrap()
                                .to_string(),
                        );
                        if seen.len() == 1 {
                            (
                                axum::http::StatusCode::BAD_GATEWAY,
                                [(axum::http::header::CONTENT_TYPE, "application/json")],
                                "{\"message\":\"temporary\"}",
                            )
                        } else {
                            (
                                axum::http::StatusCode::OK,
                                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                                "data: [DONE]\n\n",
                            )
                        }
                    },
                ),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut transport = TransportRequest {
            url: format!("http://{address}/"),
            headers: vec![("x-proof".into(), "first".into())],
            payload: "{}".into(),
            proxy: None,
            system_proxy_when_unset: false,
        };
        let telemetry = RequestTelemetry::new();
        let mut budget = RetryBudget::new(1);
        let result = send_with_retry(
            &ZCODE_ADAPTER,
            &transport,
            &mut budget,
            None,
            &telemetry,
            false,
            single_use,
            false,
        None,
    )
        .await;
        assert_eq!(budget.remaining, 0);
        if single_use {
            assert!(result.err().unwrap().rebuild);
            assert_eq!(seen.lock().unwrap().len(), 1, "禁止原样重发一次性 proof");
            transport.headers[0].1 = "second".into();
            assert!(send_with_retry(
                &ZCODE_ADAPTER,
                &transport,
                &mut budget,
                None,
                &telemetry,
                false,
                true
            , false, None)
            .await
            .is_ok());
            assert_eq!(*seen.lock().unwrap(), vec!["first", "second"]);
        } else {
            assert!(result.is_ok());
            assert_eq!(*seen.lock().unwrap(), vec!["first", "first"]);
        }
        server.abort();
    }
}

#[tokio::test]
async fn cache_creation_raw_capture_is_exact_for_prefetched_and_translated_streams() {
    use crate::server::core::{
        debug_traffic::TrafficCapture,
        upstream::{
            aggregate::aggregate_frame_stream,
            connections::{ConnectionGuard, Connections},
            translate::AnthropicToChatStream,
            usage::RequestTelemetry,
            ForwardStream,
        },
    };
    let native = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"sample\",\"usage\":{\"input_tokens\":50,\"cache_read_input_tokens\":20,\"cache_creation_input_tokens\":30}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"样例\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
    let chat = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"样例\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":20},\"cache_creation_input_tokens\":30}}\n\n",
            "data: [DONE]\n\n",
        );
    for is_native in [false, true] {
        for streaming in [false, true] {
            let body = if is_native { native } else { chat };
            let split = body.find("\n\n").unwrap() + 2;
            let app = Router::new().route(
                "/",
                post(move || async move {
                    let first = futures::stream::once(async move {
                        Ok::<_, std::io::Error>(Bytes::from_static(&body.as_bytes()[..split]))
                    });
                    let rest = futures::stream::once(async move {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Ok::<_, std::io::Error>(Bytes::from_static(&body.as_bytes()[split..]))
                    });
                    axum::body::Body::from_stream(first.chain(rest))
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let transport = TransportRequest {
                url: format!("http://{address}/"),
                headers: Vec::new(),
                payload: "{}".into(),
                proxy: None,
                system_proxy_when_unset: false,
            };
            let telemetry = Arc::new(RequestTelemetry::new());
            let capture = Arc::new(TrafficCapture::begin("sample"));
            telemetry.set_capture(capture.clone());
            let adapter = &crate::server::core::providers::zcode::adapter::ZCODE_ADAPTER;
            let mut budget = RetryBudget::new(0);
            let response = send_with_retry(
                adapter,
                &transport,
                &mut budget,
                Some(&capture),
                &telemetry,
                false,
                false,
                false,
        None,
    )
            .await
            .ok()
            .expect("本地 SSE 应通过首包检查");
            let input = if is_native {
                Box::pin(AnthropicToChatStream::from_stream(
                    response.stream,
                    "sample",
                )) as futures::stream::BoxStream<_>
            } else {
                response.stream
            };
            if streaming {
                let connection = ConnectionGuard::new(Connections::new());
                let stream = if is_native {
                    ForwardStream::from_translated(input, None, connection, telemetry.clone(), super::super::sse::FramePolicy::default())
                } else {
                    ForwardStream::from_stream(input, None, connection, telemetry.clone(), None)
                };
                let chunks = stream.try_collect::<Vec<_>>().await.unwrap();
                let output = chunks
                    .iter()
                    .flat_map(|chunk| chunk.iter().copied())
                    .collect::<Vec<_>>();
                assert!(String::from_utf8_lossy(&output).contains("样例"));
                assert!(output.ends_with(b"data: [DONE]\n\n"));
            } else {
                let result = aggregate_frame_stream(input, telemetry.clone(), super::super::sse::FramePolicy::default())
                    .await
                    .unwrap();
                assert_eq!(result.body["choices"][0]["message"]["content"], "样例");
                assert_eq!(result.body["usage"]["cache_creation_input_tokens"], 30);
            }
            assert_eq!(
                capture.captured_body(),
                body,
                "native={is_native} streaming={streaming}"
            );
            assert_eq!(telemetry.snapshot().cache_creation_tokens, Some(30));
            server.abort();
        }
    }
}

#[tokio::test]
async fn http_405_switches_accounts_without_same_account_retry() {
    use axum::http::StatusCode;

    let seen = Arc::new(Mutex::new(0usize));
    let app = Router::new()
        .route(
            "/",
            post(|State(seen): State<Arc<Mutex<usize>>>| async move {
                *seen.lock().unwrap() += 1;
                (
                    StatusCode::METHOD_NOT_ALLOWED,
                    Json(serde_json::json!({
                        "message": "request has been blocked due to unusual activity"
                    })),
                )
            }),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let transport = TransportRequest {
        url: format!("http://{address}/"),
        headers: Vec::new(),
        payload: "{}".to_string(),
        proxy: None,
        system_proxy_when_unset: false,
    };
    let adapter = &crate::server::core::providers::zcode::adapter::ZCODE_ADAPTER;
    let telemetry = crate::server::core::upstream::usage::RequestTelemetry::new();
    let mut budget = RetryBudget::new(3);

    let failure = match send_with_retry(
        adapter,
        &transport,
        &mut budget,
        None,
        &telemetry,
        false,
        false,
        false,
        None,
    )
    .await
    {
        Ok(_) => panic!("HTTP 405 should finish this account attempt"),
        Err(failure) => failure,
    };

    assert_eq!(failure.error.status_code, 405);
    assert_eq!(
        *seen.lock().unwrap(),
        1,
        "HTTP 405 must not be retried in place"
    );
    assert_eq!(
        budget.remaining, 3,
        "direct account switch must preserve resend budget"
    );
    server.abort();
}

#[tokio::test]
async fn zcode_http_200_quota_envelope_returns_a_rotatable_failure() {
    use axum::http::StatusCode;

    let app = Router::new().route(
        "/",
        post(|| async {
            (
                StatusCode::OK,
                r#"{"code":1005,"msg":"exceed quota limit"}data: {"choices":[]}"#,
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let transport = TransportRequest {
        url: format!("http://{address}/"),
        headers: Vec::new(),
        payload: "{}".to_string(),
        proxy: None,
        system_proxy_when_unset: false,
    };
    let adapter = &crate::server::core::providers::zcode::adapter::ZCODE_ADAPTER;
    let telemetry = crate::server::core::upstream::usage::RequestTelemetry::new();
    let mut budget = RetryBudget::new(0);
    let result = send_with_retry(
        adapter,
        &transport,
        &mut budget,
        None,
        &telemetry,
        false,
        false,
        false,
        None,
    )
    .await;
    let failure = match result {
        Ok(_) => panic!("HTTP 200 的 ZCode 限额信封必须回到统一失败分支"),
        Err(failure) => failure,
    };
    match failure.class {
        UpstreamErrorClass::QuotaLimited {
            status,
            upstream_code,
            ..
        } => {
            assert_eq!(status, 429);
            assert_eq!(upstream_code, Some(1005));
        }
        other => panic!("期望 QuotaLimited，得到 {other:?}"),
    }
    assert_eq!(failure.error.status_code, 429);
    server.abort();
}

// ── 会话式路径的限额记账（`mark_conversation_limited`）──────────────────────
//
// 这四条钉的是同一处缺陷：改造前该分支内联调用 `rotate::mark_account_limited`，
// `reset_at` 实参写死 `None` ⇒ 适配器算好的恢复时刻被丢在函数参数上，存储层落到
// 「缺失即 10 分钟」的兜底冷却（`account_store::store_admin::mark_rate_limited`）。
// 症状不是报错，而是**同一个已耗尽的账号按兜底常量的节拍被反复重新撞**：CodeArts
// 的福利日池要到第二天零点才有额度，生产实测两小时内白撞 8 次（每次 0.2–0.3 秒、
// 被拒不计费、整条请求仍 200 —— 只看响应看不出来）。
//
// 因此在改前的代码上第 1、4 条必红；第 2、3 条是「修复没有过头」的对照组。

static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 临时库目录，Drop 时删除。
///
/// 刻意不在建库前删了事：文件名带 pid，下一轮 pid 变了就永远删不到（曾在一台机器上
/// 攒出几百份）。调用方按 `let (_dir, service, id) = temp_service();` 接 —— 局部变量
/// 逆序析构，先关库再删目录。
struct TempDbDir(std::path::PathBuf);

impl Drop for TempDbDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 一份临时库 + 挂在其上的 [`UpstreamService`] + 里面唯一那条账号的 id。
///
/// 不发任何网络请求：记账与读回都只碰存储层，与转发选路用的是同一份读写口径。
fn temp_service() -> (TempDbDir, UpstreamService, String) {
    use crate::server::core::account_store::AccountStore;
    use crate::server::core::auth::AuthService;
    use crate::server::db::Db;

    let seq = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("pl-quota-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = AccountStore::with_db(Some(Db::open(&dir.join("agent2api.db")).expect("临时库应当能建起来")));
    let saved = store
        .add_account(
            &json!({"auth": {"accessToken": "at"}, "account": {"uid": "quota-1"}}),
            None,
            None,
        )
        .expect("临时账号应当能建起来");
    let id = saved["id"].as_str().expect("公开形态应带 id").to_string();
    let service = UpstreamService::new(store.clone(), AuthService::for_store(store));
    (TempDbDir(dir), service, id)
}

/// 本家分类结果 → 记账名单（只取本次这一个键，绕开目录缓存的全局态）。
fn single(name: &str) -> Vec<String> {
    vec![name.to_string()]
}

#[test]
fn stateful_quota_bookkeeping_keeps_the_adapter_reset_at() {
    let (_dir, service, id) = temp_service();
    let now = logging::now_ms();
    // 可辨恢复时刻：3 小时后。刻意与 10 分钟兜底不同量级 —— 改前这里落的是
    // now+600000，等值断言必红；而文案里没有任何日期形态，兜底的第二次机会
    // （`errors::parse_quota_reset_at`）也救不出这个值，差异只能来自透传。
    let reset_at = now + 3 * 60 * 60 * 1000;
    let class = UpstreamErrorClass::QuotaLimited {
        reset_at: Some(reset_at),
        message: "上游返回 403: insufficient quota".to_string(),
        upstream_code: Some(4291200),
        status: 403,
    };
    // 整组（福利池按账号记账）与单键走的是同一个循环，这里给两个名字覆盖循环体
    let group = vec!["m-benefit-a".to_string(), "m-benefit-b".to_string()];
    mark_conversation_limited(&service, &id, &group, &class, "上游返回 403: insufficient quota");
    for name in &group {
        let stored = rotate::account_limit_reset_at(&service, &id, name);
        assert_eq!(stored, reset_at, "{name} 的 resetAt 应当是适配器给的恢复时刻");
        let minutes = (stored - now) / 60_000;
        assert!(
            (stored - (now + 10 * 60 * 1000)).abs() > 60_000,
            "{name} 落到了 10 分钟兜底冷却：{minutes} 分钟"
        );
    }
    // 同一份分类里的另两个字段也照原样落地（写死 None 的那版同样丢掉过它们吗 ——
    // 没有，但这里是唯一一处能一次钉住「记账四元组一致」的地方）
    let limits = {
        let snapshot = service.store.list_accounts();
        snapshot["accounts"].as_array().unwrap()[0]["rateLimits"].clone()
    };
    assert_eq!(limits["m-benefit-a"]["status"], json!(403));
    assert_eq!(limits["m-benefit-a"]["code"], json!(4291200));
}

#[test]
fn stateful_quota_bookkeeping_falls_back_when_adapter_gives_no_reset_at() {
    let (_dir, service, id) = temp_service();
    let now = logging::now_ms();
    // 429 是分钟级限流，CodeArts 的 classify 那边就给 None（给它零点会把一个只是
    // 暂时繁忙的账号打死一整天）—— 兜底冷却是它应得的行为，修复不许把它抹掉。
    // 文案取无日期形态：解析兜底（`parse_quota_reset_at`）也救不出时刻。
    let message = "上游返回 429: too many requests";
    let class = UpstreamErrorClass::QuotaLimited {
        reset_at: None,
        message: message.to_string(),
        upstream_code: None,
        status: 429,
    };
    mark_conversation_limited(&service, &id, &single("m-429"), &class, message);
    let stored = rotate::account_limit_reset_at(&service, &id, "m-429");
    let seconds = (stored - now) / 1000;
    assert!(
        stored > now + 9 * 60 * 1000 && stored <= now + 11 * 60 * 1000,
        "无恢复时刻时应落 10 分钟兜底，实际 {seconds} 秒"
    );
}

#[test]
fn stateful_quota_bookkeeping_ignores_non_quota_classes() {
    let (_dir, service, id) = temp_service();
    // 对照组：调用方的 `if let` 已经拦下非 QuotaLimited，函数自己也不该动手 ——
    // 内容闸门/权限那一类失败本来就不该罚账号（冷却一整天尤其不能给它们）
    let message = "上游返回 403: permission denied";
    let class = UpstreamErrorClass::Fatal {
        status: 403,
        message: message.to_string(),
        upstream_code: None,
    };
    mark_conversation_limited(&service, &id, &single("m-fatal"), &class, message);
    assert_eq!(rotate::account_limit_reset_at(&service, &id, "m-fatal"), 0, "非限额分类不该落冷却");
}

#[test]
fn codearts_daily_pool_envelope_cools_to_the_boundary_instead_of_ten_minutes() {
    use crate::server::errors::GatewayError;

    let (_dir, service, id) = temp_service();
    // 生产库里那条真实文案（HTTP 200 的 SSE 里折出 403，首包门送进会话式分支）
    let error = GatewayError::with_status(403, "上游报告 InferHub.4291.200：insufficient quota");
    let class = adapter_for(ProviderKind::CodeArts).classify_conversation_error(&error);
    let UpstreamErrorClass::QuotaLimited { reset_at: Some(reset_at), .. } = &class else {
        panic!("福利池耗尽的文案应分类为带恢复时刻的 QuotaLimited，实际 {class:?}");
    };
    mark_conversation_limited(&service, &id, &single("glm-5.3-flash"), &class, &error.message);

    let stored = rotate::account_limit_reset_at(&service, &id, "glm-5.3-flash");
    assert_eq!(stored, *reset_at, "落库的恢复时刻必须是适配器算出的日池重置点");
    let minutes = (stored - logging::now_ms()) / 60_000;
    assert!(minutes > 10, "日池文案被压成 {minutes} 分钟 = 兜底常量，透传没生效");
}

#[test]
fn that_one_argument_is_the_divider_between_a_day_and_ten_minutes() {
    let (_dir, service, id) = temp_service();
    // 同一个账号库里放两条账号：一条按「改后的写法」记账，一条按「改前的写法」
    // （同一个 rotate::mark_account_limited，只是恢复时刻写死 None）。
    // 同一份分类、同一句文案 —— 差别只有那一行实参。
    let pre_fix = service
        .store
        .add_account(
            &json!({"auth": {"accessToken": "at2"}, "account": {"uid": "quota-2"}}),
            None,
            None,
        )
        .expect("对照账号应当能建起来");
    let pre_fix_id = pre_fix["id"].as_str().expect("公开形态应带 id").to_string();

    let now = logging::now_ms();
    let reset_at = now + 3 * 60 * 60 * 1000;
    let message = "上游返回 403: insufficient quota";
    let class = UpstreamErrorClass::QuotaLimited {
        reset_at: Some(reset_at),
        message: message.to_string(),
        upstream_code: Some(4291200),
        status: 403,
    };
    mark_conversation_limited(&service, &id, &single("m-after"), &class, message);
    // 改前的那一发：另三个字段照旧，恢复时刻写成 None
    rotate::mark_account_limited(&service, &pre_fix_id, "m-before", 403, Some(4291200), None, message);

    let after = rotate::account_limit_reset_at(&service, &id, "m-after");
    let before = rotate::account_limit_reset_at(&service, &pre_fix_id, "m-before");
    assert_eq!(after, reset_at, "改后应透传适配器给的恢复时刻");
    let before_minutes = (before - now) / 60_000;
    assert!(
        before_minutes >= 9 && before_minutes <= 11,
        "改前落的是存储层兜底冷却（10 分钟），实际 {before_minutes} 分钟"
    );
    // 分水岭本身：同一份分类结果，两种写法差出 2 小时以上 —— 白撞节拍就是从这里来的
    assert!(
        after - before > 2 * 60 * 60 * 1000,
        "透传与写死 None 的差距应当是「一整天 vs 10 分钟」量级，实际差 {} 分钟",
        (after - before) / 60_000
    );
}
