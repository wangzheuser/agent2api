use super::*;
use crate::server::core::auto_checkin;
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "../../../../../tests/support/checkin_http.rs"]
mod http_fixture;
use http_fixture::MockUpstream;

fn credential() -> Credential {
    Credential {
        uid: "merge-driver".into(),
        access_token: "fixture-old".into(),
        refresh_token: "fixture-refresh".into(),
        variant: "solo".into(),
        ..Default::default()
    }
}

/// 本轮签到用的设备号（账号自己的那台；见 `device_id` 的说明）。
fn device() -> String {
    device_id(&credential())
}

fn available() -> Value {
    json!({"code": 0, "data": {"enable": true, "checked_in": false, "did_checked_in": false}})
}
fn renewed() -> Value {
    json!({"Result": {"Token": "fixture-new", "RefreshToken": "fixture-next", "TokenExpireAt": 4_000_000_000_000i64}})
}
fn confirmed() -> Value {
    json!({"code": 0, "data": {"enable": true, "checked_in": false, "did_checked_in": true, "credits": 12, "extra_credits": 3}})
}

async fn drive(mock: &MockUpstream, refreshes: &AtomicUsize) -> Result<Value, GatewayError> {
    claim_with_refresh_at(credential(), None, &mock.base, || async {
        refreshes.fetch_add(1, Ordering::SeqCst);
        // 使用真实换证解析与 HTTP 层，但候选端点仅限本地，失败不回退生产 host。
        let next = super::super::refresh::refresh_once(
            &credential(),
            &[format!("{}/refresh", mock.base)],
            None,
        )
        .await?;
        Ok((next, None))
    })
    .await
}

#[tokio::test]
async fn trae_driver_stops_on_any_completed_flag_without_claim_or_refresh() {
    for fields in [
        json!({"checked_in": false, "did_checked_in": true, "enable": false}),
        json!({"checked_in": true, "did_checked_in": false, "enable": false}),
        json!({"already_claimed": false, "hasClaimed": true, "enable": true}),
    ] {
        for nested in [false, true] {
            let body = if nested {
                json!({"code": 0, "checked_in": false, "data": fields})
            } else {
                let mut body = fields.clone();
                body["code"] = json!(0);
                body
            };
            let mock = MockUpstream::spawn(vec![(200, body)]).await;
            let refreshes = AtomicUsize::new(0);
            let result = drive(&mock, &refreshes).await.unwrap();
            assert_eq!(result["alreadyClaimed"], true);
            assert_eq!(result["success"], false);
            assert_eq!(refreshes.load(Ordering::SeqCst), 0);
            let requests = mock.requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method, "POST");
            assert_eq!(requests[0].path, STATUS_PATH);
            assert_eq!(requests[0].headers["x-device-id"], device());
            assert_eq!(requests[0].body, "{}");
            mock.assert_drained();
        }
    }
}

#[tokio::test]
async fn trae_driver_no_activity_is_neutral_without_claim_or_refresh() {
    for nested in [false, true] {
        let fields = json!({"checked_in": false, "did_checked_in": false, "enable": false});
        let body = if nested {
            json!({"code": 0, "data": fields})
        } else {
            let mut body = fields;
            body["code"] = json!(0);
            body
        };
        let mock = MockUpstream::spawn(vec![(200, body)]).await;
        let refreshes = AtomicUsize::new(0);
        let result = drive(&mock, &refreshes).await.unwrap();
        assert_eq!(result["success"], false);
        assert_eq!(result["status"], "unsupported");
        assert_eq!(result["checkinDeviceId"], device());
        for field in [
            "awarded",
            "checkinCredits",
            "checkinExtraCredits",
            "alreadyClaimed",
            "alreadyCompleted",
            "rewardPoints",
            "raw",
        ] {
            assert!(
                result.get(field).is_none(),
                "无活动不能携带完成或奖励字段：{field}"
            );
        }
        let summary = json!({"results": [{"id": "trae-fixture", "provider": "trae", "claim": result, "error": null}]});
        assert!(auto_checkin::failed_account_labels(&summary).is_empty());
        assert!(auto_checkin::completed_account_ids(&summary).is_empty());
        assert_eq!(refreshes.load(Ordering::SeqCst), 0);
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, STATUS_PATH);
        mock.assert_drained();
    }
}

#[tokio::test]
async fn trae_driver_already_completed_preserves_h_and_upstream_reward_fields() {
    for fields in [
        json!({"checked_in": false, "did_checked_in": true, "enable": false, "credits": 12, "extra_credits": 3}),
        json!({"checked_in": true, "did_checked_in": false, "enable": true, "credits": 12, "extra_credits": 3}),
        json!({"alreadyClaimed": true, "enable": false, "credits": 12, "extra_credits": 3}),
    ] {
        for nested in [false, true] {
            let body = if nested {
                json!({"code": 0, "data": fields})
            } else {
                let mut body = fields.clone();
                body["code"] = json!(0);
                body
            };
            let mock = MockUpstream::spawn(vec![(200, body)]).await;
            let refreshes = AtomicUsize::new(0);
            let result = drive(&mock, &refreshes).await.unwrap();
            assert_eq!(result["success"], false);
            assert_eq!(result["status"], "already_claimed");
            assert_eq!(result["alreadyClaimed"], true);
            assert_eq!(result["alreadyCompleted"], true);
            assert_eq!(result["awarded"], 15);
            assert_eq!(result["checkinCredits"], 12);
            assert_eq!(result["checkinExtraCredits"], 3);
            assert_eq!(result["checkinDeviceId"], device());
            let summary = json!({"results": [{"id": "trae-fixture", "provider": "trae", "claim": result, "error": null}]});
            assert!(auto_checkin::failed_account_labels(&summary).is_empty());
            assert_eq!(
                auto_checkin::completed_account_ids(&summary),
                ["trae-fixture"]
            );
            assert_eq!(refreshes.load(Ordering::SeqCst), 0);
            let requests = mock.requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].path, STATUS_PATH);
            mock.assert_drained();
        }
    }
}

#[tokio::test]
async fn trae_driver_refreshes_only_confirmation_on_the_claim_device() {
    for auth in [
        (401, json!({})),
        (200, json!({"code": 1001})),
        (200, json!({"code": 401})),
    ] {
        let claim = json!({"code": 0, "reward_points": 12});
        let mock = MockUpstream::spawn(vec![
            (200, available()),
            (200, claim.clone()),
            auth,
            (200, renewed()),
            (200, confirmed()),
        ])
        .await;
        let refreshes = AtomicUsize::new(0);
        let result = drive(&mock, &refreshes).await.unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["status"], "claimed");
        assert_eq!(result["rewardPoints"], 12);
        assert_eq!(result["raw"], claim);
        assert_eq!(result["reauthAttempted"], true);
        assert_eq!(result["awarded"], 15);
        assert_eq!(result["checkinCredits"], 12);
        assert_eq!(result["checkinExtraCredits"], 3);
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        let requests = mock.requests();
        assert_eq!(
            requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            [STATUS_PATH, CLAIM_PATH, STATUS_PATH, "/refresh", STATUS_PATH]
        );
        // 三发（以及换发后的重放）都落在**同一个**设备号上：换号回查等于自证失败，
        // 而 9074 之后换号也不再是"重试"，是假身份。
        for index in [0, 1, 2, 4] {
            assert_eq!(requests[index].headers["x-device-id"], device());
        }
        assert_eq!(
            requests[2].headers["authorization"],
            "Cloud-IDE-JWT fixture-old"
        );
        assert_eq!(
            requests[4].headers["authorization"],
            "Cloud-IDE-JWT fixture-new"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&requests[3].body).unwrap()["RefreshToken"],
            "fixture-refresh"
        );
        mock.assert_drained();
    }
}

#[tokio::test]
async fn trae_driver_repeated_confirmation_auth_stops_after_one_refresh() {
    for auth in [(401, json!({})), (200, json!({"code": 1001}))] {
        let mock = MockUpstream::spawn(vec![
            (200, available()),
            (200, json!({"code": 0})),
            auth.clone(),
            (200, renewed()),
            auth,
        ])
        .await;
        let refreshes = AtomicUsize::new(0);
        assert_eq!(drive(&mock, &refreshes).await.unwrap_err().status_code, 401);
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        let requests = mock.requests();
        assert_eq!(
            requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            [
                STATUS_PATH,
                CLAIM_PATH,
                STATUS_PATH,
                "/refresh",
                STATUS_PATH
            ]
        );
        assert_eq!(
            requests[4].headers["x-device-id"],
            requests[1].headers["x-device-id"]
        );
        mock.assert_drained();
    }
}

#[tokio::test]
async fn trae_driver_preclaim_auth_refreshes_once_and_shares_the_budget() {
    for after in [confirmed(), json!({"code": 1001})] {
        let mock = MockUpstream::spawn(vec![
            (401, json!({})),
            (200, renewed()),
            (200, available()),
            (200, json!({"code": 0})),
            (200, after.clone()),
        ])
        .await;
        let refreshes = AtomicUsize::new(0);
        let result = drive(&mock, &refreshes).await;
        if after["code"] == 0 {
            assert_eq!(result.unwrap()["success"], true);
        } else {
            assert_eq!(result.unwrap_err().status_code, 401);
        }
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        let requests = mock.requests();
        assert_eq!(
            requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            [
                STATUS_PATH,
                "/refresh",
                STATUS_PATH,
                CLAIM_PATH,
                STATUS_PATH
            ]
        );
        assert_eq!(
            requests[2].headers["authorization"],
            "Cloud-IDE-JWT fixture-new"
        );
        mock.assert_drained();
    }
}

#[tokio::test]
async fn trae_driver_failed_refresh_after_claim_never_reclaims() {
    let mock = MockUpstream::spawn(vec![
        (200, available()),
        (200, json!({"code": 0})),
        (401, json!({})),
        (401, json!({"code": 1001})),
    ])
    .await;
    let refreshes = AtomicUsize::new(0);
    assert_eq!(drive(&mock, &refreshes).await.unwrap_err().status_code, 401);
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        mock.requests()
            .iter()
            .map(|r| r.path.as_str())
            .collect::<Vec<_>>(),
        [STATUS_PATH, CLAIM_PATH, STATUS_PATH, "/refresh"]
    );
    mock.assert_drained();
}

#[tokio::test]
async fn trae_driver_backoff_keeps_h_status_and_upstream_device_diagnostics() {
    // 9074 一发即收口：状态接口撞到就只打一发，领取撞到也只打两发。
    // 上一版会在状态接口撞到后换个设备号再打一发 —— 那既不是重试也不是同一台设备。
    let cases: [(&[(&str, Value)], &[&str]); 2] = [
        (
            &[(STATUS_PATH, json!({"code": 9074}))],
            &[STATUS_PATH],
        ),
        (
            &[
                (STATUS_PATH, available()),
                (CLAIM_PATH, json!({"code": 9074})),
            ],
            &[STATUS_PATH, CLAIM_PATH],
        ),
    ];
    for (script, paths) in cases {
        // 脚本按顺序回放：这里只关心发了几发、每发的路径，状态码都是 200。
        let mock = MockUpstream::spawn(script.iter().map(|(_, body)| (200, body.clone())).collect()).await;
        let refreshes = AtomicUsize::new(0);
        let result = drive(&mock, &refreshes).await.unwrap();
        assert_eq!(result["success"], false);
        assert_eq!(result["status"], "backoff");
        assert_eq!(result["code"], 9074);
        assert_eq!(result["checkinDeviceId"], device());
        assert!(
            !result["msg"]
                .as_str()
                .unwrap_or_default()
                .contains("设备限流"),
            "9074 不是限流，文案不能这么写：{}",
            result["msg"]
        );
        assert!(result.get("alreadyCompleted").is_none());
        assert!(result.get("awarded").is_none());
        let summary =
            json!({"results": [{"id": "trae-fixture", "provider": "trae", "claim": result}]});
        assert!(auto_checkin::completed_account_ids(&summary).is_empty());
        assert_eq!(auto_checkin::failed_account_labels(&summary).len(), 1);
        assert_eq!(refreshes.load(Ordering::SeqCst), 0);
        assert_eq!(
            mock.requests()
                .iter()
                .map(|request| request.path.as_str())
                .collect::<Vec<_>>(),
            paths
        );
        for request in mock.requests() {
            assert_eq!(request.headers["x-device-id"], device(), "一轮只用一台设备");
        }
        mock.assert_drained();
    }
}

#[tokio::test]
async fn trae_driver_unconfirmed_does_not_export_claim_rewards_as_completion() {
    let mock = MockUpstream::spawn(vec![
        (200, available()),
        (200, json!({"code": 0, "reward_points": 999})),
        (200, available()),
    ])
    .await;
    let refreshes = AtomicUsize::new(0);
    let result = drive(&mock, &refreshes).await.unwrap();
    assert_eq!(result["success"], false);
    assert_eq!(result["claimUnconfirmed"], true);
    assert_eq!(result["checkinDeviceId"], device());
    for field in [
        "awarded",
        "checkinCredits",
        "checkinExtraCredits",
        "alreadyClaimed",
        "alreadyCompleted",
        "rewardPoints",
        "raw",
    ] {
        assert!(
            result.get(field).is_none(),
            "未确认不能携带完成或奖励字段：{field}"
        );
    }
    let summary = json!({"results": [{"id": "trae-fixture", "provider": "trae", "claim": result}]});
    assert!(auto_checkin::completed_account_ids(&summary).is_empty());
    assert_eq!(auto_checkin::failed_account_labels(&summary).len(), 1);
    assert_eq!(refreshes.load(Ordering::SeqCst), 0);
    let requests = mock.requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        [STATUS_PATH, CLAIM_PATH, STATUS_PATH]
    );
    for request in requests {
        assert_eq!(request.headers["x-device-id"], device());
    }
    mock.assert_drained();
}

// ── 端到端（真 SQLite + 真 HTTP，只把上游换成回环 mock）────────────
//
// 上面那些用例直接喂 claim_with_refresh_at，绕过了「账号记录 ↔ 凭据」
// 这一层；而本轮改的恰恰是设备身份（credential.device_id 决定签到号）
// 与续期回写。这两件事只有在真账号记录上才验得出来。
//
// 隔离纪律：api_host 必须指向 mock。续期候选表是「api_host + 两个兜底域名」，
// 指向真域名就意味着一条断言的失败会真的把假令牌发出去
// （v0 踩过这个坑，见 refresh.rs 的同类用例）。

#[path = "../../../../../tests/support/checkin_http.rs"]
mod http_fixture_e2e;
use http_fixture_e2e::MockUpstream as E2eUpstream;

/// 造一条**已落库**的 Trae 凭据。api_host 恒为 mock，expires_at 决定本轮
/// 要不要走续期。
fn e2e_credential(
    pair: &super::super::device::DeviceKeyPair,
    api_host: &str,
    expires_at: i64,
) -> Credential {
    Credential {
        uid: "e2e-user".into(),
        access_token: "e2e-old".into(),
        refresh_token: "e2e-refresh".into(),
        expires_at,
        domain: "trae.cn".into(),
        api_host: api_host.to_string(),
        machine_id: "e2e-machine".into(),
        device_id: "6273884102398761".into(),
        variant: "solo".into(),
        device_public_key: pair.public_pem.clone(),
        device_private_key: pair.private_pem.clone(),
        ..Default::default()
    }
}

fn e2e_store(
    label: &str,
    credential: &Credential,
) -> (AccountStore, String, crate::server::db::test_temp::TempDb) {
    // 每个用例一个库：几条用例并发跑，共用同名文件会互相删库。
    let (db, guard) = crate::server::db::test_temp::TempDb::open(label);
    let store = AccountStore::with_db(Some(db));
    let added = store
        .add_trae_account(credential, None, "test")
        .expect("账号应能落库");
    let id = added["account"]["id"]
        .as_str()
        .or_else(|| added["id"].as_str())
        .expect("落库返回里要有 id")
        .to_string();
    assert!(
        store.trae_account_record(&id).is_some(),
        "按 id 查不回账号记录：{id}"
    );
    (store, id, guard)
}

fn minutes_from_now(minutes: i64) -> i64 {
    crate::server::logging::now_ms() + minutes * 60_000
}

/// 签到端到端：过期账号先按客户端方式续期（带 DeviceInfo + DeviceProof），
/// 再用**落库的那台设备**发 status → claim → 回查，三发同号。
#[tokio::test]
async fn trae_e2e_checkin_renews_with_device_proof_then_claims_on_the_stored_device() {
    let pair = super::super::device::generate_device_key_pair().expect("生成应当成功");
    let mock = E2eUpstream::spawn(vec![
        // 1) 设备续期（带签名的 v3 端点）
        (
            200,
            json!({"Result": {"Token": "e2e-bound", "RefreshToken": "e2e-next", "TokenExpireAt": 4_000_000_000_000i64}}),
        ),
        // 2) status → 3) claim → 4) 同设备回查
        (200, available()),
        (200, json!({"code": 0})),
        (200, confirmed()),
    ])
    .await;
    let credential = e2e_credential(&pair, &mock.base, crate::server::logging::now_ms() - 60_000);
    let (store, id, _guard) = e2e_store("trae-checkin-e2e-claim", &credential);

    let result = claim_daily_at(&store, &id, &mock.base)
        .await
        .expect("签到应当返回结果");
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(result["status"], "claimed", "{result}");
    assert_eq!(result["awarded"], 15, "{result}");
    // 用的是账号记录里那台设备，不是按 uid 现造的号。
    assert_eq!(result["checkinDeviceId"], "6273884102398761");

    let requests = mock.requests();
    assert_eq!(
        requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        [
            "/trae/api/v3/oauth/ExchangeToken",
            STATUS_PATH,
            CLAIM_PATH,
            STATUS_PATH
        ],
        "先按客户端方式续期，再走 status/claim/回查"
    );

    // 续期那一发：设备身份与签名都在，且签名能被落盘的公钥验过。
    let renew: Value = serde_json::from_str(&requests[0].body).expect("续期体是 JSON");
    assert_eq!(renew["DeviceInfo"]["DeviceID"], "6273884102398761");
    assert_eq!(renew["DeviceInfo"]["MachineID"], "e2e-machine");
    assert_eq!(renew["DeviceInfo"]["DevicePublicKey"], pair.public_pem);
    assert_eq!(renew["RefreshToken"], "e2e-refresh");
    assert!(renew["DeviceProof"]["Signature"]
        .as_str()
        .is_some_and(|text| !text.is_empty()));

    // 签名必须能用**落盘在账号记录里**的那把公钥验过：这一步把
    // 「SQLite 往返」与「签名/验签」串成一条，光看字段非空证明不了。
    {
        use base64::Engine as _;
        use p256::ecdsa::signature::Verifier;
        use p256::pkcs8::DecodePublicKey;

        let record = store.trae_account_record(&id).expect("记录应在");
        let stored_public = record["devicePublicKey"].as_str().expect("公钥应已落盘");
        let signature = base64::engine::general_purpose::STANDARD
            .decode(renew["DeviceProof"]["Signature"].as_str().unwrap_or_default())
            .expect("签名是 base64");
        let message = super::super::refresh::device_proof_message(
            super::super::refresh::DEVICE_REFRESH_PATH,
            renew["ClientID"].as_str().unwrap_or_default(),
            "e2e-refresh",
            renew["DeviceProof"]["Timestamp"]
                .as_i64()
                .expect("时间戳是整数"),
            renew["DeviceProof"]["Nonce"].as_str().unwrap_or_default(),
        );
        let verifying = p256::ecdsa::VerifyingKey::from_public_key_pem(stored_public)
            .expect("落盘公钥可解");
        let parsed = p256::ecdsa::Signature::from_der(&signature).expect("DER 签名可解");
        verifying
            .verify(message.as_bytes(), &parsed)
            .expect("签名应当能被落盘公钥验过");
    }

    // 签到三发：同一设备号、空体。
    for request in &requests[1..] {
        assert_eq!(request.headers["x-device-id"], "6273884102398761");
        assert_eq!(request.body, "{}", "签到体恒为空对象");
        assert_eq!(
            request.headers["authorization"],
            "Cloud-IDE-JWT e2e-bound",
            "续期后的新令牌要立刻用于签到"
        );
    }

    // 续期结果真的回写了（下一轮不必再换）。
    let record = store.trae_account_record(&id).expect("记录应在");
    assert_eq!(record["accessToken"], "e2e-bound");
    assert_eq!(record["refreshToken"], "e2e-next");
    assert_eq!(record["deviceId"], "6273884102398761", "续期不许改设备绑定");
    mock.assert_drained();
}

/// 9074 端到端：只发一发（状态接口），不换号、不重试、文案不说"限流"。
#[tokio::test]
async fn trae_e2e_9074_stops_after_one_request_without_rotating_the_device() {
    let pair = super::super::device::generate_device_key_pair().expect("生成应当成功");
    let mock = E2eUpstream::spawn(vec![(
        200,
        json!({"code": 9074, "message": "当前参与用户太多，请稍后再试"}),
    )])
    .await;
    // 30 天后到期：本轮**不触发续期**，只验签到本身的设备号与 9074 收口。
    // （续期失败会回退到真实 api.trae.cn，测试里必须避开那条分支。）
    let credential = e2e_credential(&pair, &mock.base, minutes_from_now(30 * 24 * 60));
    let (store, id, _guard) = e2e_store("trae-checkin-e2e-9074", &credential);

    let result = claim_daily_at(&store, &id, &mock.base)
        .await
        .expect("应当返回可读结果");
    assert_eq!(result["code"], 9074, "{result}");
    assert_eq!(result["status"], "backoff", "{result}");
    assert_eq!(result["checkinDeviceId"], "6273884102398761", "{result}");
    let message = result["msg"].as_str().unwrap_or_default();
    assert!(!message.contains("设备限流"), "9074 不是限流：{message}");
    assert!(message.contains("不是限流"), "{message}");

    let requests = mock.requests();
    assert_eq!(1, requests.len(), "9074 一发即收口，不换号重试");
    assert_eq!(requests[0].headers["x-device-id"], "6273884102398761");
    mock.assert_drained();
}

/// 9095 端到端：设备今日已签 → 记为已完成，不是失败。
#[tokio::test]
async fn trae_e2e_9095_is_reported_as_completed_not_failed() {
    let pair = super::super::device::generate_device_key_pair().expect("生成应当成功");
    let mock = E2eUpstream::spawn(vec![
        (200, available()),
        (200, json!({"code": 9095, "message": "当前设备今日已经签到"})),
    ])
    .await;
    // 同上：不触发续期，避免回退候选打到真实上游。
    let credential = e2e_credential(&pair, &mock.base, minutes_from_now(30 * 24 * 60));
    let (store, id, _guard) = e2e_store("trae-checkin-e2e-9095", &credential);

    let result = claim_daily_at(&store, &id, &mock.base)
        .await
        .expect("应当返回可读结果");
    assert_eq!(result["code"], 9095, "{result}");
    assert_eq!(result["alreadyCompleted"], true, "{result}");
    let summary = json!({"results": [{"id": id, "provider": "trae", "claim": result}]});
    assert_eq!(auto_checkin::completed_account_ids(&summary), [id.clone()]);
    assert!(auto_checkin::failed_account_labels(&summary).is_empty());
    mock.assert_drained();
}
