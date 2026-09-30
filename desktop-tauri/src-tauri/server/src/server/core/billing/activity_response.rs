//! 活跃保活必须收到有效回答并正常结束；HTTP 200 或读到 EOF 都不代表成功。

use futures::StreamExt;
use serde_json::Value;

pub(super) async fn consume_activity_response(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<(), &'static str> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "响应流读取失败")?;
        if chunk.len() > max_bytes.saturating_sub(body.len()) {
            return Err("响应超过大小限制");
        }
        body.extend_from_slice(&chunk);
    }
    validate_activity_sse(&body)
}

fn validate_activity_sse(body: &[u8]) -> Result<(), &'static str> {
    let text = std::str::from_utf8(body).map_err(|_| "响应不是有效 UTF-8")?;
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut answered = false;
    let mut finished = false;
    let mut done = false;
    for frame in normalized.split("\n\n") {
        let mut data = Vec::new();
        for line in frame.lines() {
            if let Some(value) = line.strip_prefix("data:") {
                data.push(value.strip_prefix(' ').unwrap_or(value));
            } else if let Some(event) = line.strip_prefix("event:") {
                if event.trim().eq_ignore_ascii_case("error") {
                    return Err("上游返回错误事件");
                }
            } else if !line.is_empty()
                && !line.starts_with(':')
                && !line.starts_with("id:")
                && !line.starts_with("retry:")
            {
                return Err("响应不是有效 SSE");
            }
        }
        if data.is_empty() {
            continue;
        }
        let data = data.join("\n");
        if data.trim() == "[DONE]" {
            done = true;
            continue;
        }
        let value: Value = serde_json::from_str(&data).map_err(|_| "SSE 数据不是有效 JSON")?;
        // 错误正文可能包含敏感信息，调用方只记录固定原因。
        for payload in [&value, &value["data"]] {
            if payload.get("error").is_some_and(|error| !error.is_null())
                || payload.get("code").is_some_and(|code| {
                    !code.is_null() && code.as_i64() != Some(0) && code.as_str() != Some("0")
                })
            {
                return Err("上游返回业务错误");
            }
        }
        if done {
            return Err("结束标记后仍有业务数据");
        }
        if let Some(choice) = value["choices"].get(0) {
            if choice["delta"]["content"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty())
            {
                if finished {
                    return Err("回答结束后仍有内容");
                }
                answered = true;
            }
            // WorkBuddy 在中间帧使用空字符串，语义与 OpenAI 的 null 一致。
            if let Some(reason) = choice["finish_reason"].as_str().filter(|s| !s.is_empty()) {
                // 保活请求限制输出长度，正常的 length 收尾也可接受，仍须有正文。
                if !matches!(reason, "stop" | "length") {
                    return Err("回答未正常结束");
                }
                finished = true;
            }
        }
    }
    if !answered {
        return Err("未收到有效回答");
    }
    if !finished && !done {
        return Err("响应提前结束");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str =
        "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"},\"finish_reason\":null}]}\n\n";
    const STOP: &str = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";

    #[test]
    fn accepts_answer_with_normal_completion() {
        for suffix in [STOP, "data: [DONE]\n\n"] {
            assert_eq!(
                validate_activity_sse(format!("{ANSWER}{suffix}").as_bytes()),
                Ok(())
            );
        }
        assert_eq!(
            validate_activity_sse(format!("{ANSWER}{STOP}data: [DONE]\n\n").as_bytes()),
            Ok(())
        );
    }

    #[test]
    fn accepts_workbuddy_empty_finish_reason_until_stop() {
        let answer = ANSWER.replace("null", "\"\"");
        assert_eq!(
            validate_activity_sse(format!("{answer}{STOP}data: [DONE]\n\n").as_bytes()),
            Ok(())
        );
        assert!(validate_activity_sse(answer.as_bytes()).is_err());
    }

    #[test]
    fn rejects_empty_error_truncated_and_reasoning_only_responses() {
        for body in [
            "", "{}", "data: [DONE]\n\n", ANSWER,
            "data: {\"error\":{\"message\":\"failed\"}}\n\ndata: [DONE]\n\n",
            "data: {\"code\":403,\"data\":null}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\ndata: [DONE]\n\n",
        ] {
            assert!(validate_activity_sse(body.as_bytes()).is_err(), "{body}");
        }
    }

    #[test]
    fn rejects_errors_even_after_an_answer_or_terminal_marker() {
        for suffix in [
            "data: {\"choices\":[{\"finish_reason\":\"content_filter\"}]}\n\n",
            "data: {invalid}\n\n",
            "event: error\ndata: {}\n\n",
            "data: [DONE]\n\ndata: {\"error\":\"failed\"}\n\n",
        ] {
            assert!(validate_activity_sse(format!("{ANSWER}{suffix}").as_bytes()).is_err());
        }
    }

    fn response(chunks: Vec<Vec<u8>>) -> reqwest::Response {
        let stream = futures::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
        axum::http::Response::new(reqwest::Body::wrap_stream(stream)).into()
    }

    #[tokio::test]
    async fn handles_crlf_and_split_utf8_bytes() {
        let body = format!(": heartbeat\n\n{ANSWER}{STOP}").replace('\n', "\r\n");
        let chunks = body
            .as_bytes()
            .chunks(1)
            .map(|part| part.to_vec())
            .collect();
        assert_eq!(
            consume_activity_response(response(chunks), body.len()).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn rejects_oversized_response_instead_of_reporting_success() {
        let body = format!("{ANSWER}{STOP}").into_bytes();
        let max_bytes = body.len() - 1;
        assert_eq!(
            consume_activity_response(response(vec![body]), max_bytes).await,
            Err("响应超过大小限制")
        );
    }
}
