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
            assert_eq!(
                requests[0].headers["x-device-id"],
                device_id("merge-driver", 0)
            );
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
        assert_eq!(result["checkinDeviceId"], device_id("merge-driver", 0));
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
            assert_eq!(result["checkinDeviceId"], device_id("merge-driver", 0));
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
            (200, json!({"code": 9074})),
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
            [
                STATUS_PATH,
                STATUS_PATH,
                CLAIM_PATH,
                STATUS_PATH,
                "/refresh",
                STATUS_PATH
            ]
        );
        assert_eq!(
            requests[0].headers["x-device-id"],
            device_id("merge-driver", 0)
        );
        for index in [1, 2, 3, 5] {
            assert_eq!(
                requests[index].headers["x-device-id"],
                device_id("merge-driver", 1)
            );
        }
        assert_eq!(
            requests[3].headers["authorization"],
            "Cloud-IDE-JWT fixture-old"
        );
        assert_eq!(
            requests[5].headers["authorization"],
            "Cloud-IDE-JWT fixture-new"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&requests[4].body).unwrap()["RefreshToken"],
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
    for (script, paths, generation) in [
        (
            vec![(200, json!({"code": 9074})), (200, json!({"code": 9074}))],
            [STATUS_PATH, STATUS_PATH],
            1,
        ),
        (
            vec![(200, available()), (200, json!({"code": 9074}))],
            [STATUS_PATH, CLAIM_PATH],
            0,
        ),
    ] {
        let mock = MockUpstream::spawn(script).await;
        let refreshes = AtomicUsize::new(0);
        let result = drive(&mock, &refreshes).await.unwrap();
        assert_eq!(result["success"], false);
        assert_eq!(result["status"], "backoff");
        assert_eq!(result["code"], 9074);
        assert_eq!(
            result["checkinDeviceId"],
            device_id("merge-driver", generation)
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
    assert_eq!(result["checkinDeviceId"], device_id("merge-driver", 0));
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
        assert_eq!(request.headers["x-device-id"], device_id("merge-driver", 0));
    }
    mock.assert_drained();
}
