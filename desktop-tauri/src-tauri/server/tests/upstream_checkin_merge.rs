//! 本轮签到合并的独立契约；只构造请求和本地数据库，不调用上游。
use agent2api_server::server::core::{
    account_store::AccountStore,
    auth::AuthService,
    auto_checkin,
    billing::{checkin, keepalive, BillingService, WorkbuddyActivity},
    checkin_history, custom_providers,
    providers::{
        trae::checkin::{self as trae, Award, Outcome, Probe},
        workbuddy::keepalive::build_daily_activity_request,
    },
    upstream::request::TransportRequest,
};
use agent2api_server::server::{config, db::Db};
use serde_json::{json, Value};
use std::collections::BTreeSet;

#[test]
fn trae_status_preserves_h_aliases_and_reads_upstream_award_fields() {
    for fields in [
        json!({"checked_in": true, "enable": true, "credits": 12, "extra_credits": 3}),
        json!({"checked_in": false, "did_checked_in": true, "enable": true, "credits": 12, "extra_credits": 3}),
    ] {
        for nested in [false, true] {
            let mut payload = if nested {
                json!({"data": fields})
            } else {
                fields.clone()
            };
            payload["code"] = json!(0);
            payload["message"] = json!("  fixture status  ");
            let status = trae::status_of(&payload);
            assert!(status.done_today(), "{payload}");
            assert!(status.enable);
            assert_eq!(
                status.award(),
                Award {
                    credits: 12,
                    extra_credits: 3
                }
            );
            assert_eq!(status.award().total(), 15);
            assert_eq!(status.message, "fixture status");
        }
    }
    for key in [
        "already_claimed",
        "alreadyClaimed",
        "claimed",
        "has_claimed",
        "hasClaimed",
    ] {
        let mut payload = json!({"code": 0, "data": {}});
        payload["data"][key] = json!(true);
        assert!(trae::status_of(&payload).done_today(), "alias {key}");
    }
    for key in ["can_claim", "canClaim", "claimable", "available", "enable"] {
        let mut payload = json!({"code": 0, "data": {}});
        payload["data"][key] = json!(false);
        assert!(!trae::status_of(&payload).enable, "alias {key}");
    }
    // H 缺少资格字段时仍允许探测，不能因 U 的缺省值静默停签。
    assert!(trae::status_of(&json!({"code": 0})).enable);
}

#[test]
fn trae_failed_or_missing_business_code_never_confirms_completion() {
    for code in [json!(1001), json!(9074), Value::Null] {
        let status = trae::status_of(&json!({
            "code": code, "enable": true, "checked_in": true, "did_checked_in": true,
        }));
        assert!(!status.done_today(), "{code}");
    }
    assert!(!trae::status_of(&json!({})).done_today());
}

#[test]
fn trae_decide_requires_successful_confirmation_and_uses_its_award() {
    let before = trae::status_of(&json!({"code": 0, "enable": true}));
    let claim = trae::status_of(&json!({"code": 0, "credits": 999}));
    assert_eq!(trae::decide(&before, &claim, None), Outcome::Unconfirmed);
    for after in [
        json!({"code": 0, "checked_in": false}),
        json!({"code": 9074, "checked_in": true}),
        json!({"code": 1001, "did_checked_in": true}),
        json!({"checked_in": true}),
    ] {
        assert_eq!(
            trae::decide(&before, &claim, Some(&trae::status_of(&after))),
            Outcome::Unconfirmed,
            "{after}"
        );
    }
    for key in ["checked_in", "did_checked_in"] {
        let mut after = json!({"code": 0, "data": {"credits": 12, "extra_credits": 3}});
        after["data"][key] = json!(true);
        assert_eq!(
            trae::decide(&before, &claim, Some(&trae::status_of(&after))),
            Outcome::Claimed(Award {
                credits: 12,
                extra_credits: 3
            })
        );
    }
    let zero_award = trae::status_of(&json!({"code": 0, "checked_in": true}));
    assert_eq!(
        trae::decide(&before, &claim, Some(&zero_award)),
        Outcome::Claimed(Award {
            credits: 0,
            extra_credits: 0
        })
    );
}

#[test]
fn trae_decide_preserves_already_completed_and_rejection_precedence() {
    let ready = trae::status_of(&json!({"code": 0, "enable": true}));
    let rejected = trae::status_of(&json!({"code": 9074, "message": "fixture rejected"}));
    assert_eq!(
        trae::decide(&ready, &rejected, Some(&ready)),
        Outcome::Rejected(9074, "fixture rejected".into())
    );
    assert_eq!(
        trae::decide(&rejected, &ready, None),
        Outcome::Rejected(9074, "fixture rejected".into())
    );
    let unavailable = trae::status_of(&json!({"code": 0, "enable": false}));
    assert_eq!(
        trae::decide(&unavailable, &ready, None),
        Outcome::NotEnabled
    );
    // H 先识别已签到；活动关闭不能把已经完成的账号退回未完成。
    for enabled in [true, false] {
        let completed = trae::status_of(
            &json!({"code": 0, "enable": enabled, "checked_in": true, "credits": 5, "extra_credits": 2}),
        );
        assert_eq!(
            trae::decide(&completed, &rejected, None),
            Outcome::AlreadyCheckedIn(Award {
                credits: 5,
                extra_credits: 2
            })
        );
    }
}

#[test]
fn trae_auth_expiry_and_device_backoff_remain_distinct() {
    for row in [
        json!({"code": 401}),
        json!({"code": 1001}),
        json!({"msg": "not able to authenticate"}),
        json!({"msg": "unable to authenticate"}),
    ] {
        assert!(trae::needs_reauth(&row), "{row}");
    }
    for row in [
        json!({"code": 9074}),
        json!({"code": 500}),
        json!({"code": 0}),
        json!({}),
    ] {
        assert!(!trae::needs_reauth(&row), "{row}");
    }
    for (code, expected) in [
        (0, Probe::Accept),
        (9074, Probe::NextBody),
        (1001, Probe::GiveUp),
        (500, Probe::NextScheme),
    ] {
        assert_eq!(trae::probe_for(code), expected);
    }
    assert_eq!(trae::device_id("u1", 0), "4302850041909017");
    let next = trae::device_id("u1", 1);
    assert_ne!(next, trae::device_id("u1", 0));
    assert_eq!(next, trae::device_id("u1", 1));
    assert_eq!(next.len(), 16);
    assert!(next.bytes().all(|byte| byte.is_ascii_digit()));
}

#[test]
fn workbuddy_activity_modes_round_trip_and_unknown_values_default_to_full() {
    for (text, mode) in [
        ("full", WorkbuddyActivity::Full),
        ("claim", WorkbuddyActivity::Claim),
        ("keepalive", WorkbuddyActivity::Keepalive),
    ] {
        assert_eq!(WorkbuddyActivity::parse(Some(text)), mode);
        assert_eq!(mode.as_str(), text);
    }
    for value in [None, Some(""), Some("unknown")] {
        assert_eq!(WorkbuddyActivity::parse(value), WorkbuddyActivity::Full);
    }
}

#[test]
fn checkin_provider_union_retains_all_twelve_options() {
    let expected = [
        "workbuddy",
        "workbuddy-intl",
        "raccoon",
        "autoclaw",
        "autoclaw-intl",
        "qoder",
        "qoder-intl",
        "trae",
        "minimax-code",
        "lobsterai",
        "reward-custom",
        "loomy",
        "kuku",
    ];
    let defaults = auto_checkin::default_providers();
    assert_eq!(defaults.len(), 13);
    assert_eq!(
        defaults.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(expected)
    );
    assert_eq!(auto_checkin::normalize_providers(None), defaults);
    for value in [json!(null), json!([]), json!(["unknown", 1, false])] {
        assert_eq!(auto_checkin::normalize_providers(Some(&value)), defaults);
    }
    let subset = auto_checkin::normalize_providers(Some(&json!([
        "kuku",
        "reward-custom",
        "kuku",
        "unknown"
    ])));
    assert_eq!(subset.len(), 2);
    assert_eq!(
        subset.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(["kuku", "reward-custom"])
    );
}

#[test]
fn checkin_eligibility_preserves_native_rewards_and_region_boundaries() {
    for (provider, edition) in [
        ("workbuddy", "cn"),
        ("workbuddy", "intl"),
        ("workbuddy-intl", "intl"),
        ("raccoon", "cn"),
        ("autoclaw", "cn"),
        ("autoclaw-intl", "intl"),
        ("qoder", "cn"),
        ("qoder", "intl"),
        ("trae", "cn"),
        ("minimax-code", "cn"),
        ("minimax-code", "intl"),
        ("lobsterai", "cn"),
        ("lobsterai", "intl"),
        ("loomy", "cn"),
        ("kuku", "cn"),
    ] {
        assert!(
            checkin::supports_checkin(&json!({"provider": provider, "edition": edition})),
            "{provider}/{edition}"
        );
    }
    for (provider, edition) in [
        ("trae", "intl"),
        ("raccoon", "intl"),
        ("codearts", "cn"),
        ("accio", "cn"),
        ("accio-intl", "intl"),
        ("custom-fixture", "cn"),
    ] {
        assert!(
            !checkin::supports_checkin(&json!({"provider": provider, "edition": edition})),
            "{provider}/{edition}"
        );
    }
}

fn header<'a>(request: &'a TransportRequest, name: &str) -> Option<&'a str> {
    let mut values = request
        .headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name));
    let first = values.next().map(|(_, value)| value.as_str());
    assert!(values.next().is_none(), "重复请求头: {name}");
    first
}

#[test]
fn workbuddy_keepalive_preserves_h_region_headers_proxy_and_minimal_body() {
    for (provider, edition, international) in [
        (Some("workbuddy"), "cn", false),
        (Some("workbuddy-intl"), "intl", true),
        (Some("workbuddy"), "intl", false),
        (Some("workbuddy-intl"), "cn", true),
        (None, "intl", true),
        (None, "cn", false),
    ] {
        let mut session = json!({
            "provider": provider, "edition": edition, "endpoint": "https://fixture.invalid/proxy/",
            "auth": {"accessToken": "fixture-token", "domain": "fixture-domain"},
            "account": {"uid": "fixture-user", "enterpriseId": "fixture-enterprise"},
            "proxy": {"source": "custom", "protocol": "socks5", "host": "127.0.0.1", "port": 19080,
                "username": "fixture-user", "password": "fixture-password", "label": "fixture"},
        });
        let original = session.clone();
        let request = build_daily_activity_request(&session, "fixture-free-model").unwrap();
        assert_eq!(session, original, "构造请求不改账号会话");
        assert_eq!(
            request.url,
            "https://fixture.invalid/proxy/v2/chat/completions"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&request.payload).unwrap(),
            json!({
                "model": "fixture-free-model", "stream": true, "max_tokens": 16,
                "messages": [{"role": "system", "content": "You are a helpful assistant."}, {"role": "user", "content": "hi"}],
            })
        );
        assert_eq!(header(&request, "Accept"), Some("text/event-stream"));
        assert_eq!(
            header(&request, "Authorization"),
            Some("Bearer fixture-token")
        );
        assert_eq!(header(&request, "X-User-Id"), Some("fixture-user"));
        assert_eq!(
            header(&request, "X-Enterprise-Id"),
            Some("fixture-enterprise")
        );
        assert_eq!(header(&request, "X-Domain"), Some("fixture-domain"));
        assert_eq!(
            header(&request, "X-IDE-Type"),
            Some("WorkBuddy"),
            "{provider:?}/{edition}"
        );
        let product = if international {
            "WorkBuddy AI"
        } else {
            "WorkBuddy"
        };
        assert_eq!(
            header(&request, "X-IDE-Name"),
            Some(product),
            "{provider:?}/{edition}"
        );
        assert_eq!(
            header(&request, "X-Product"),
            Some(product),
            "{provider:?}/{edition}"
        );
        if international {
            assert_eq!(header(&request, "X-Agent-Intent"), None);
            assert_eq!(header(&request, "X-Agent-Purpose"), Some("conversation"));
            assert_eq!(header(&request, "Origin"), Some("https://www.workbuddy.ai"));
            assert_eq!(
                header(&request, "Referer"),
                Some("https://www.workbuddy.ai/")
            );
            assert_eq!(header(&request, "X-Requested-With"), Some("XMLHttpRequest"));
            let request_id = header(&request, "X-Request-ID").unwrap();
            assert!(!request_id.is_empty());
            assert_eq!(
                header(&request, "X-Conversation-Message-ID"),
                Some(request_id)
            );
            assert_eq!(header(&request, "X-Root-Request-ID"), Some(request_id));
        } else {
            assert_eq!(header(&request, "X-Agent-Intent"), Some("craft"));
            for key in ["X-Agent-Purpose", "Origin", "Referer", "X-Requested-With"] {
                assert_eq!(header(&request, key), None, "{key}");
            }
        }
        let proxy = request.proxy.as_ref().expect("保活必须沿用账号代理");
        assert_eq!(proxy.source, "custom");
        assert_eq!(proxy.protocol, "socks5");
        assert_eq!(proxy.host, "127.0.0.1");
        assert_eq!(proxy.port, Some(19080));
        assert_eq!(proxy.username, "fixture-user");
        assert_eq!(proxy.password, "fixture-password");
        assert_eq!(proxy.label, "fixture");
        session.as_object_mut().unwrap().remove("proxy");
        assert!(build_daily_activity_request(&session, "fixture-free-model")
            .unwrap()
            .proxy
            .is_none());
    }
}

#[test]
fn completed_account_ids_only_include_confirmed_rows_from_this_run() {
    let result = json!({
        "succeeded": 99, "skipped": 7, "selectedAccountIds": ["not-executed"],
        "results": [
            {"id": "claimed", "claim": {"success": true}},
            {"id": "already", "claim": {"success": false, "alreadyCompleted": true}},
            {"id": "h-already", "claim": {"status": "already_claimed"}},
            {"id": "h-flag", "claim": {"alreadyClaimed": true}},
            {"id": "claimed", "claim": {"success": true}},
            {"id": "keepalive", "activity": {"pokeSucceeded": true}, "claim": {"success": false}},
            {"id": "failed", "error": "HTTP 401", "claim": {"success": true}},
            {"id": "nested-error", "claim": {"success": true, "error": "HTTP 401"}},
            {"id": "skipped", "skipped": true, "claim": {"success": true}},
            {"id": "claim-skipped", "claim": {"success": true, "skipped": true}},
            {"id": "unconfirmed", "claim": {"success": false, "claimUnconfirmed": true}},
            {"id": "no-activity", "claim": {"success": false, "msg": "当前没有可领取的签到活动"}},
            {"id": "no-claim", "success": true},
            {"id": " ", "claim": {"success": true}},
        ],
    });
    assert_eq!(
        auto_checkin::completed_account_ids(&result),
        vec!["claimed", "already", "h-already", "h-flag"]
    );
    assert!(auto_checkin::completed_account_ids(&json!({"succeeded": 1})).is_empty());
}

#[test]
fn authentication_failures_remain_visible_after_successful_keepalive() {
    let result = json!({"results": [
        {"name": "top-auth", "error": "HTTP 401", "activity": {"pokeSucceeded": true}},
        {"name": "claim-auth", "claim": {"success": false, "code": 401, "msg": "登录态过期"}, "activity": {"pokeSucceeded": true}},
        {"name": "token-dead", "provider": "trae", "claim": {"success": false, "code": 1001}, "activity": {"pokeSucceeded": true}},
        {"name": "message-auth", "claim": {"success": false, "msg": "unable to authenticate"}, "activity": {"pokeSucceeded": true}},
        {"name": "neutral", "claim": {"success": false, "code": 0, "msg": "网页会话完成，日活奖励尚未确认"}, "activity": {"pokeSucceeded": true}},
        {"name": "failed-poke", "claim": {"success": false, "msg": "网页保活失败"}, "activity": {"pokeSucceeded": false}},
    ]});
    assert_eq!(
        auto_checkin::failed_account_labels(&result),
        vec![
            "top-auth（HTTP 401）",
            "claim-auth（登录态过期）",
            "token-dead（签到领取失败）",
            "message-auth（unable to authenticate）",
            "failed-poke（网页保活失败）",
        ]
    );
}

#[test]
fn provider_business_code_1001_preserves_completion_without_hiding_trae_auth() {
    let result = json!({"results": [
        {"id": "wb", "provider": "workbuddy", "claim": {"success": false, "alreadyCompleted": true, "code": 1001, "msg": "今日已签到"}},
        {"id": "legacy", "claim": {"success": false, "alreadyCompleted": true, "code": 1001, "msg": "今日已签到"}},
        {"id": "trae", "provider": "trae", "claim": {"success": false, "alreadyCompleted": true, "code": 1001, "msg": "今日已签到"}, "activity": {"pokeSucceeded": true}},
        {"id": "auth", "provider": "workbuddy", "claim": {"success": false, "alreadyCompleted": true, "code": 401, "msg": "今日已签到"}, "activity": {"pokeSucceeded": true}},
    ]});
    assert_eq!(
        auto_checkin::completed_account_ids(&result),
        ["wb", "legacy"]
    );
    assert_eq!(
        auto_checkin::failed_account_labels(&result),
        ["trae（今日已签到）", "auth（今日已签到）"]
    );
}

#[test]
fn keepalive_persistence_and_reward_eligibility_use_isolated_sqlite() {
    // 与现有独立测试共用 fixture 机制；本测试二进制中只有此用例访问全局 config。
    let dir = std::env::temp_dir().join(format!(
        "upstream-checkin-merge-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    let db = Db::open(&dir.join("fixture.db")).unwrap();
    config::init(Some(db.clone()));
    assert_eq!(keepalive::models(), keepalive::default_models());
    assert!(config::update_raw_field(
        "fixtureSentinel",
        json!({"preserve": true})
    ));
    let expected = vec!["fixture-model-b".to_string(), "fixture-model-a".to_string()];
    let saved = keepalive::set_models(&[
        " fixture-model-b ".into(),
        "".into(),
        "fixture-model-a".into(),
        "fixture-model-b".into(),
    ])
    .unwrap();
    assert_eq!(saved, expected);
    assert_eq!(keepalive::models(), expected);
    assert_eq!(
        keepalive::state(),
        json!({"models": expected, "defaultModels": keepalive::default_models()})
    );
    assert_eq!(
        config::current().raw().get("checkinKeepalive"),
        Some(&json!({"models": expected}))
    );
    // 另开连接读实际落盘内容，不能只依赖 set_models 的返回值或内存快照。
    let disk = Db::open(&dir.join("fixture.db")).unwrap();
    let persisted: String = disk
        .with(|conn| {
            conn.query_row(
                "SELECT value FROM kv WHERE key = 'checkinKeepalive'",
                [],
                |row| row.get(0),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&persisted).unwrap(),
        json!({"models": expected})
    );
    config::reload();
    assert_eq!(keepalive::models(), expected);
    let mut raw = config::current().raw().clone();
    raw.insert(
        "checkinKeepalive".into(),
        json!({"models": ["fixture-from-disk"]}),
    );
    assert!(config::save_raw(&raw));
    assert_eq!(keepalive::models(), expected, "save_raw 尚未刷新快照");
    config::reload();
    assert_eq!(keepalive::models(), vec!["fixture-from-disk"]);
    assert_eq!(
        config::current().raw().get("fixtureSentinel"),
        Some(&json!({"preserve": true}))
    );
    for invalid in [
        Value::Null,
        json!({"models": "not-an-array"}),
        json!({"models": [" ", null, 1]}),
    ] {
        assert!(config::update_raw_field("checkinKeepalive", invalid));
        assert_eq!(keepalive::models(), keepalive::default_models());
    }
    assert_eq!(
        keepalive::set_models(&[" ".into()]).unwrap(),
        keepalive::default_models()
    );
    config::reload();
    assert_eq!(keepalive::models(), keepalive::default_models());

    let store = AccountStore::with_db(Some(db));
    let mut selected_ids = BTreeSet::new();
    for profile in ["astudio", "dumate"] {
        let provider = custom_providers::create(&json!({"name": format!("fixture-{profile}"), "protocol": "chat_completions", "baseUrl": "https://fixture.invalid/v1", "rewardProfile": profile})).unwrap();
        let provider_id = provider["id"].as_str().unwrap();
        let model_only = store
            .add_custom_account(
                provider_id,
                &json!({"apiKey": format!("fixture-model-only-{profile}")}),
                None,
            )
            .unwrap();
        let opted_in = store
            .add_custom_account(
                provider_id,
                &json!({"apiKey": format!("fixture-model-{profile}")}),
                None,
            )
            .unwrap();
        let id = opted_in["id"].as_str().unwrap();
        assert_eq!(
            checkin::resolve_checkin_targets(&store, &[], Some(id))
                .unwrap_err()
                .status_code,
            400
        );
        store
            .set_custom_reward_credential(id, &format!("fixture-reward-{profile}"))
            .unwrap();
        assert!(
            !checkin::supports_checkin(&opted_in),
            "自定义奖励资格由存储中的显式凭证决定"
        );
        assert_eq!(
            checkin::resolve_checkin_targets(&store, &[], Some(model_only["id"].as_str().unwrap()))
                .unwrap_err()
                .status_code,
            400
        );
        selected_ids.insert(id.to_string());
    }
    let (targets, skipped) =
        checkin::resolve_checkin_targets(&store, &["reward-custom".into()], None).unwrap();
    assert_eq!(
        targets
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_string())
            .collect::<BTreeSet<_>>(),
        selected_ids
    );
    assert_eq!(skipped, 2);
    let (targets, skipped) =
        checkin::resolve_checkin_targets(&store, &["workbuddy".into()], None).unwrap();
    assert!(targets.is_empty());
    assert_eq!(skipped, 4);
    for id in selected_ids {
        store.set_custom_reward_credential(&id, "").unwrap();
        assert_eq!(
            checkin::resolve_checkin_targets(&store, &[], Some(&id))
                .unwrap_err()
                .status_code,
            400
        );
    }
    assert_history_uses_scheduler_failure_semantics();
    assert_legacy_and_explicit_provider_selection(&store);
}

fn assert_history_uses_scheduler_failure_semantics() {
    checkin_history::record(
        &json!({
            "succeeded": 2, "active": 1, "total": 7, "skipped": 3,
            "results": [
                {"name": "already", "error": "HTTP 400: 今天已签到"},
                {"name": "completed", "claim": {"success": false, "alreadyCompleted": true, "msg": "fixture completed"}},
                {"name": "inactive", "claim": {"success": false, "msg": "当前没有可领取的奖励"}},
                {"name": "no-daily", "claim": {"success": false, "msg": "无每日签到活动"}},
                {"name": "active", "activity": {"pokeSucceeded": true}, "claim": {"success": false, "msg": "日活奖励尚未确认"}},
                {"name": "expired", "error": "HTTP 401: 凭证已过期"},
                {"id": "rejected", "claim": {"success": false, "msg": "官方拒绝领取"}},
            ],
        }),
        "fixture",
    );
    let history = checkin_history::list();
    assert_eq!(history[0]["failedCount"], 2);
    assert_eq!(
        history[0]["failed"],
        json!([
            "expired（HTTP 401: 凭证已过期）",
            "rejected（官方拒绝领取）"
        ])
    );
    assert_eq!(history[0]["succeeded"], 2, "中性结果不增加领取数");
    assert_eq!(history[0]["active"], 1);
    assert_eq!(history[0]["total"], 7);
    assert_eq!(history[0]["skipped"], 3);
    let failures: Vec<Value> = (0..7)
        .map(|i| json!({"id": format!("failure-{i}"), "error": "fixture failure"}))
        .collect();
    checkin_history::record(&json!({"results": failures}), "fixture-many");
    config::reload();
    let history = checkin_history::list();
    assert_eq!(history[0]["failedCount"], 7);
    assert_eq!(history[0]["failed"].as_array().unwrap().len(), 5);
    assert_eq!(history[1]["reason"], "fixture");
}

fn assert_legacy_and_explicit_provider_selection(store: &AccountStore) {
    let service = auto_checkin::AutoCheckin::new(
        store.clone(),
        BillingService::new(AuthService::for_store(store.clone())),
    );
    for (legacy, expected) in [
        (json!(["workbuddy"]), json!(["workbuddy", "workbuddy-intl"])),
        (
            json!(["workbuddy", "trae"]),
            json!(["workbuddy", "workbuddy-intl", "trae"]),
        ),
        (json!(["workbuddy-intl"]), json!(["workbuddy-intl"])),
        (json!(["trae"]), json!(["trae"])),
    ] {
        assert!(config::update_raw_field(
            "autoCheckin",
            json!({"enabled": false, "providers": legacy})
        ));
        assert_eq!(service.state()["providers"], expected);
    }
    assert!(config::update_raw_field(
        "autoCheckin",
        json!({
            "enabled": false, "providers": ["workbuddy"], "fixtureSentinel": "preserve",
            "lastFiredDate": "2000-01-01", "lastResult": {"failedCount": 1},
        })
    ));
    // 仅改时间不能把旧 workbuddy 解释提前标记为新版精确选择。
    assert_eq!(
        service.configure(&json!({"time": "00:02"})).unwrap()["providers"],
        json!(["workbuddy", "workbuddy-intl"])
    );
    let cn = store.add_account(&json!({"edition": "cn", "auth": {"accessToken": "fixture-cn"}, "account": {"uid": "checkin-cn"}}), None, Some("workbuddy")).unwrap();
    let intl = store.add_account(&json!({"edition": "intl", "auth": {"accessToken": "fixture-intl"}, "account": {"uid": "checkin-intl"}}), None, Some("workbuddy-intl")).unwrap();
    let (legacy_targets, _) =
        checkin::resolve_checkin_targets(store, &service.configured_providers(), None).unwrap();
    assert_eq!(legacy_targets.len(), 2);
    for (provider, account) in [("workbuddy-intl", intl), ("workbuddy", cn)] {
        assert_eq!(
            service
                .configure(&json!({"providers": [provider]}))
                .unwrap()["providers"],
            json!([provider])
        );
        config::reload();
        assert_eq!(service.configured_providers(), vec![provider.to_string()]);
        assert_eq!(
            service.configure(&json!({"time": "00:03"})).unwrap()["providers"],
            json!([provider])
        );
        let (targets, _) =
            checkin::resolve_checkin_targets(store, &service.configured_providers(), None).unwrap();
        assert_eq!(
            targets
                .iter()
                .map(|row| row["id"].clone())
                .collect::<Vec<_>>(),
            vec![account["id"].clone()],
            "取消另一地区后实际目标也必须移除"
        );
    }
    let snapshot = config::current();
    let saved = snapshot.raw().get("autoCheckin").unwrap();
    assert_eq!(saved["fixtureSentinel"], "preserve");
    assert_eq!(saved["lastFiredDate"], "2000-01-01");
    assert_eq!(saved["lastResult"], json!({"failedCount": 1}));
    assert!(service.configure(&json!({"providers": []})).is_err());
    assert_eq!(service.configured_providers(), vec!["workbuddy"]);
}
