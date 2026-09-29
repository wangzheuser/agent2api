//! Headless 的验证码生产者：官方 SDK 在隔离浏览器中运行，通过管道交付 proof。
//! 不新增 HTTP 端点，不给子进程管理员口令、API Key 或账号 JWT。
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::server::core::{
    providers::zcode::{captcha, claim, region::Region},
    proxies,
};
use crate::server::{api::zcode_captcha, logging, ServerState};

struct Worker {
    child: Child,
    #[cfg(unix)]
    process_group: Option<u32>,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // 子进程异常退出也收拢同组 Chromium/Xvfb，防止重启留下孤儿进程。
        #[cfg(unix)]
        if let Some(id) = self.process_group {
            let _ = std::process::Command::new("/bin/kill")
                .args(["-TERM", "--", &format!("-{id}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

impl Worker {
    fn spawn(script: &str) -> Result<Self, &'static str> {
        let mut command = Command::new("node");
        // 默认继承会把服务端管理员密码一起交给 SDK 宿主；仅保留运行环境。
        command.env_clear();
        for name in [
            "PATH",
            "HOME",
            "LANG",
            "LC_ALL",
            "TZ",
            "TMPDIR",
            "AGENT2API_CHROMIUM_PATH",
            "AGENT2API_CAPTCHA_UI_SCRIPT",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|_| "worker_start_failed")?;
        let input = child.stdin.take().ok_or("worker_stdin_missing")?;
        let output = BufReader::new(child.stdout.take().ok_or("worker_stdout_missing")?);
        #[cfg(unix)]
        let process_group = child.id();
        Ok(Self {
            child,
            #[cfg(unix)]
            process_group,
            input: Some(input),
            output,
        })
    }

    async fn mint(&mut self, config: &Value) -> Result<(String, String), &'static str> {
        tokio::time::timeout(Duration::from_secs(55), async {
            let mut command = serde_json::to_vec(config).map_err(|_| "config_invalid")?;
            command.push(b'\n');
            let input = self.input.as_mut().ok_or("worker_closed")?;
            input
                .write_all(&command)
                .await
                .map_err(|_| "worker_write_failed")?;
            input.flush().await.map_err(|_| "worker_write_failed")?;
            let mut line = String::new();
            self.output
                .read_line(&mut line)
                .await
                .map_err(|_| "worker_read_failed")?;
            let response: Value =
                serde_json::from_str(&line).map_err(|_| "worker_response_invalid")?;
            if response.get("error").and_then(Value::as_str) == Some("sdk_rejected_F001") {
                return Err("sdk_rejected_F001");
            }
            let param = response.get("param").and_then(Value::as_str).unwrap_or("");
            if !(200..=16384).contains(&param.len()) {
                return Err("sdk_failed");
            }
            Ok((
                param.into(),
                config
                    .get("region")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
            ))
        })
        .await
        .map_err(|_| "worker_timeout")?
    }

    async fn stop(&mut self) {
        self.input.take(); // EOF 通知 Node 关闭 Chromium，再退出。
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

async fn configuration(state: &ServerState, account_id: &str) -> Result<Value, &'static str> {
    let record = state
        .store()
        .zcode_account_record(account_id)
        .ok_or("account_missing")?;
    let region = record
        .get("provider")
        .and_then(Value::as_str)
        .and_then(Region::from_provider_id)
        .ok_or("provider_invalid")?;
    let proxy = state
        .store()
        .get_session_by_id(account_id)
        .and_then(|item| proxies::session_proxy(&item.session));
    let config = claim::captcha_config(region, proxy.as_ref())
        .await
        .map_err(|_| "config_fetch_failed")?
        .filter(|c| c.enabled)
        .ok_or("config_disabled")?;
    let mut result =
        json!({"prefix": config.prefix, "sceneId": config.scene_id, "region": config.region});
    if let Some(p) = proxy {
        result["proxy"] = json!({
            "server": format!("{}://{}:{}", p.protocol, p.host, p.port.unwrap_or(80)),
            "username": p.username, "password": p.password,
        });
    }
    Ok(result)
}

/// 只由 headless 入口启动；Docker 设置脚本路径，桌面继续使用网页生产者。
pub async fn run(state: ServerState, script: String) {
    let mut worker: Option<Worker> = None;
    let mut cached: Option<(String, Instant, Value)> = None;
    let mut failures = 0u64;
    loop {
        let (count, account_id) = zcode_captcha::start_plan_accounts(&state);
        if count == 0 {
            if let Some(mut old) = worker.take() {
                old.stop().await;
            }
            cached = None;
            captcha::set_producer("idle", failures);
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        let stats = captcha::stats();
        if stats.get("fresh").and_then(Value::as_u64).unwrap_or(0) >= captcha::POOL_TARGET as u64 {
            captcha::set_producer("ready", failures);
            tokio::select! {
                _ = captcha::wait_for_demand() => {},
                _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            }
            continue;
        }
        captcha::set_producer("minting", failures);
        let result = async {
            let id = account_id.as_deref().ok_or("account_missing")?;
            if !cached
                .as_ref()
                .is_some_and(|(old, at, _)| old == id && at.elapsed() < Duration::from_secs(300))
            {
                cached = Some((id.into(), Instant::now(), configuration(&state, id).await?));
            }
            if worker.is_none() {
                worker = Some(Worker::spawn(&script)?);
            }
            let config = &cached.as_ref().ok_or("config_missing")?.2;
            worker.as_mut().ok_or("worker_missing")?.mint(config).await
        }
        .await;
        match result {
            Ok((param, region)) => {
                let ready = captcha::push(&param, &region);
                logging::console_line(
                    "[ZCodeCaptcha]",
                    &format!("server minted ready={ready} recovered={}", failures > 0),
                );
                failures = 0;
                captcha::set_producer("ready", failures);
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            Err(code) => {
                failures = failures.saturating_add(1);
                captcha::set_producer(code, failures);
                logging::log(
                    "[ZCodeCaptcha]",
                    &format!("producer failure={code} consecutive={failures}; restarting"),
                );
                if let Some(mut old) = worker.take() {
                    old.stop().await;
                }
                cached = None;
                tokio::time::sleep(Duration::from_secs((failures * 2).min(15))).await;
            }
        }
    }
}
