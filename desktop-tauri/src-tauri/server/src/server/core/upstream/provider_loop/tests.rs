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
            , false),
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
            , false)
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
                    ForwardStream::from_translated(input, None, connection, telemetry.clone(), None)
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
                let result = aggregate_frame_stream(input, telemetry.clone(), None)
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
