//! LobsterAI 账号存储：手动凭证、网页登录结果和公开形态。
//!
//! LobsterAI 的 refresh 结果由 provider 模块通过 `update_account_tokens` 回写；本文件
//! 只负责把嵌套/平铺凭证归一成项目通用的账号记录，避免另建平行账号库。

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::server::core::providers::lobsterai::credentials::Credentials;
use crate::server::logging;

use super::priority::next_free_priority;
use super::sql;
use super::state::StoredAccount;
use super::store::{AccountStore, AccountStoreError};
use super::store_util::{max_concurrent_public, token_tail_of, truncate_chars};
use super::LOBSTERAI_PROVIDER_ID;

impl AccountStore {
    /// 取 LobsterAI 原始账号记录；空 id 表示本家队首账号。
    pub fn lobsterai_account_record(&self, account_id: &str) -> Option<Value> {
        let guard = self.guard();
        if !account_id.trim().is_empty() {
            let record = self.record_by_id(&guard, account_id)?;
            return (record.provider() == LOBSTERAI_PROVIDER_ID).then(|| record.to_value());
        }
        self.records_for_provider(&guard, LOBSTERAI_PROVIDER_ID)
            .into_iter()
            .filter(|record| record.enabled() && record.has_credentials())
            .min_by_key(|record| record.order_key())
            .map(|record| record.to_value())
    }

    /// 添加或更新一条 LobsterAI 账号。
    pub fn add_lobsterai_account(
        &self,
        credentials: &Credentials,
        name: Option<&str>,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        if credentials.access_token.trim().is_empty() {
            return Err(AccountStoreError::bad_request("缺少 LobsterAI accessToken"));
        }
        let identity = if !credentials.user_id.trim().is_empty() {
            credentials.user_id.trim().to_string()
        } else if !credentials.uid.trim().is_empty() {
            credentials.uid.trim().to_string()
        } else {
            let digest = Sha256::digest(credentials.access_token.as_bytes());
            digest.iter().map(|byte| format!("{byte:02x}")).collect()
        };
        let id = format!("lobsterai-{identity}");
        let guard = self.guard();
        let existing = self.record_by_id(&guard, &id);
        if let Some(record) = existing.as_ref() {
            if record.provider() != LOBSTERAI_PROVIDER_ID {
                return Err(AccountStoreError::new(
                    format!("账号 id「{id}」已被{}账号占用", record.provider()),
                    409,
                ));
            }
        }
        let priority = if let Some(record) = existing.as_ref() {
            record.priority()
        } else {
            let used = self
                .with_conn(&guard, sql::priorities_all)
                .map_err(|error| AccountStoreError::new(error.to_string(), 500))?;
            next_free_priority(&used)
        };
        let display_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, 100))
            .or_else(|| {
                (!credentials.nickname.trim().is_empty())
                    .then(|| truncate_chars(credentials.nickname.trim(), 100))
            })
            .unwrap_or_else(|| format!("LobsterAI {identity}"));
        let now = logging::now_ms();
        let mut fields = existing
            .as_ref()
            .map(StoredAccount::fields)
            .cloned()
            .unwrap_or_default();
        fields.insert("id".to_string(), Value::String(id.clone()));
        fields.insert(
            "provider".to_string(),
            Value::String(LOBSTERAI_PROVIDER_ID.to_string()),
        );
        fields.insert("name".to_string(), Value::String(display_name));
        fields.insert(
            "uid".to_string(),
            Value::String(if credentials.uid.trim().is_empty() {
                identity.clone()
            } else {
                credentials.uid.clone()
            }),
        );
        fields.insert(
            "userId".to_string(),
            Value::String(credentials.user_id.clone()),
        );
        fields.insert(
            "nickname".to_string(),
            Value::String(credentials.nickname.clone()),
        );
        fields.insert("uuid".to_string(), Value::String(credentials.uuid.clone()));
        fields.insert(
            "firstKeyfrom".to_string(),
            Value::String(credentials.first_keyfrom.clone()),
        );
        fields.insert(
            "latestKeyfrom".to_string(),
            Value::String(credentials.latest_keyfrom.clone()),
        );
        fields.insert(
            "accessToken".to_string(),
            Value::String(credentials.access_token.clone()),
        );
        fields.insert(
            "refreshToken".to_string(),
            Value::String(credentials.refresh_token.clone()),
        );
        fields.insert(
            "tokenTail".to_string(),
            Value::String(token_tail_of(&credentials.access_token)),
        );
        if let Some(expires_at) = credentials.expires_at_ms {
            fields.insert("expiresAt".to_string(), Value::from(expires_at));
        }
        if let Some(refresh_expires_at) = credentials.refresh_expires_at_ms {
            fields.insert(
                "refreshExpiresAt".to_string(),
                Value::from(refresh_expires_at),
            );
        }
        fields.insert("source".to_string(), Value::String(source.to_string()));
        fields.insert("priority".to_string(), Value::from(priority));
        fields.insert(
            "enabled".to_string(),
            Value::Bool(
                existing
                    .as_ref()
                    .map(StoredAccount::enabled)
                    .unwrap_or(true),
            ),
        );
        fields.insert(
            "addedAt".to_string(),
            Value::from(
                existing
                    .as_ref()
                    .map(StoredAccount::added_at)
                    .unwrap_or(now),
            ),
        );
        fields.insert("updatedAt".to_string(), Value::from(now));
        fields.insert("rateLimits".to_string(), json!({}));
        let record = StoredAccount::from_map(fields);
        self.with_conn(&guard, |conn| sql::put(conn, &record))?;
        Ok(self.to_lobsterai_public_account(&record))
    }

    /// LobsterAI 账号公开形态，不返回 token，只返回尾号和刷新状态。
    pub(crate) fn to_lobsterai_public_account(&self, record: &StoredAccount) -> Value {
        let mut public = Map::new();
        for key in [
            "id",
            "provider",
            "name",
            "uid",
            "userId",
            "nickname",
            "source",
            "tokenTail",
            "expiresAt",
            "refreshExpiresAt",
        ] {
            public.insert(
                key.to_string(),
                record.get(key).cloned().unwrap_or(Value::Null),
            );
        }
        public.insert(
            "hasRefreshToken".to_string(),
            Value::Bool(!record.refresh_token().is_empty()),
        );
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert(
            "proxy".to_string(),
            crate::server::core::proxies::describe_account_proxy(Some(&record.proxy())),
        );
        public.insert(
            "available".to_string(),
            Value::Bool(record.has_credentials()),
        );
        public.insert(
            "maxConcurrent".to_string(),
            Value::from(max_concurrent_public(record.get("maxConcurrent"))),
        );
        Value::Object(public)
    }
}
