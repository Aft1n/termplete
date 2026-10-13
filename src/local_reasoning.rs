//! Resolve local models' thinking controls once per model, then reuse them.

use std::collections::HashMap;

use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::config::{ProviderPreset, Wire};
use crate::{Error, Result};

#[derive(Default)]
pub(crate) struct Controls {
    efforts: Mutex<HashMap<(String, String), Option<String>>>,
}

impl Controls {
    pub(crate) fn prepare(
        &self,
        http: &reqwest::blocking::Client,
        preset: &ProviderPreset,
        model: &str,
        api_key: Option<&str>,
        url: &mut String,
        body: &mut Value,
    ) -> Result<bool> {
        let ollama = preset.wire == Wire::Ollama;
        let lmstudio = preset.name == "lmstudio" && preset.wire == Wire::OpenAiCompat;
        if !ollama && !lmstudio {
            return Ok(false);
        }
        let base = preset.base_url.trim_end_matches('/');
        let base = base.strip_suffix("/v1").unwrap_or(base);
        let key = (base.to_string(), model.to_string());
        let cached = self.efforts.lock().get(&key).cloned();
        let effort = match cached {
            Some(effort) => effort,
            None => {
                let mut request = if ollama {
                    http.post(format!("{base}/api/show"))
                        .json(&json!({ "model": model }))
                } else {
                    http.get(format!("{base}/api/v1/models"))
                };
                if let Some(key) = api_key {
                    request = request.bearer_auth(key);
                }
                let response = request.send().map_err(|error| {
                    Error::Network(format!(
                        "could not read local model thinking controls: {error}"
                    ))
                })?;
                let effort = if response.status() == reqwest::StatusCode::NOT_FOUND {
                    // Older servers have no discovery endpoint. Ollama's
                    // compatibility aliases still support these two settings.
                    if ollama {
                        Some(
                            if model.contains("gpt-oss") {
                                "low"
                            } else {
                                "none"
                            }
                            .into(),
                        )
                    } else {
                        None
                    }
                } else {
                    let metadata: Value = response
                        .error_for_status()
                        .and_then(reqwest::blocking::Response::json)
                        .map_err(|error| {
                            Error::Provider(format!("could not read local model metadata: {error}"))
                        })?;
                    if !metadata.is_object() {
                        return Err(Error::Provider(
                            "local model metadata must be a JSON object".into(),
                        ));
                    }
                    let values = if ollama {
                        metadata
                            .pointer("/thinking/values")
                            .and_then(Value::as_array)
                    } else {
                        metadata
                            .get("models")
                            .and_then(Value::as_array)
                            .and_then(|models| {
                                models.iter().find(|entry| {
                                    entry.get("key").and_then(Value::as_str) == Some(model)
                                        || entry
                                            .get("loaded_instances")
                                            .and_then(Value::as_array)
                                            .is_some_and(|instances| {
                                                instances.iter().any(|instance| {
                                                    instance.get("id").and_then(Value::as_str)
                                                        == Some(model)
                                                })
                                            })
                                })
                            })
                            .and_then(|entry| {
                                entry.pointer("/capabilities/reasoning/allowed_options")
                            })
                            .and_then(Value::as_array)
                    };
                    match values {
                        Some(values) => {
                            let effort = if ollama && values.contains(&Value::Bool(false)) {
                                Some("none")
                            } else {
                                ["off", "none", "low", "minimal"]
                                    .into_iter()
                                    .find(|effort| {
                                        values.iter().any(|value| value.as_str() == Some(*effort))
                                    })
                            };
                            Some(effort.ok_or_else(|| Error::Provider(format!(
                                "model `{model}` cannot disable reasoning or use Low; choose another model"
                            )))?.to_string())
                        }
                        None if ollama => Some(
                            if model.contains("gpt-oss") {
                                "low"
                            } else {
                                "none"
                            }
                            .into(),
                        ),
                        None => None,
                    }
                };
                self.efforts.lock().insert(key, effort.clone());
                effort
            }
        };
        if ollama {
            if let Some(effort) = effort {
                if effort == "low" || effort == "minimal" {
                    if let Some(object) = body.as_object_mut() {
                        object.remove("stop");
                        let max_tokens = object
                            .get("max_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(96)
                            .max(512);
                        object.insert("max_tokens".into(), json!(max_tokens));
                    }
                }
                body["reasoning_effort"] = json!(effort);
            }
            return Ok(false);
        }
        let Some(effort) = effort else {
            return Ok(false);
        };
        // LM Studio exposes thinking controls on its native API. Preserve the
        // supplied context, without enabling integrations or retaining a chat.
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Provider("missing local completion messages".into()))?;
        let mut system = Vec::new();
        let mut input = Vec::new();
        for message in messages {
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match message.get("role").and_then(Value::as_str) {
                Some("system") => system.push(content.to_string()),
                Some("assistant") => input.push(format!("Previous response:\n{content}")),
                _ => input.push(content.to_string()),
            }
        }
        if body
            .pointer("/response_format/type")
            .and_then(Value::as_str)
            == Some("json_object")
        {
            system.push("Return only a valid JSON object.".to_string());
        }
        let max_tokens = body.get("max_tokens").and_then(Value::as_u64).unwrap_or(96);
        // Required reasoning shares the generation budget with the answer.
        let max_tokens = if effort == "low" || effort == "minimal" {
            max_tokens.max(512)
        } else {
            max_tokens
        };
        let mut native = json!({
            "model": model,
            "system_prompt": system.join("\n\n"),
            "input": input.join("\n\n"),
            "reasoning": effort,
            "stream": false,
            "store": false,
            "integrations": [],
            "max_output_tokens": max_tokens
        });
        for field in ["temperature", "top_p"] {
            if let Some(value) = body.get(field) {
                native[field] = value.clone();
            }
        }
        *body = native;
        *url = format!("{base}/api/v1/chat");
        Ok(true)
    }
}

pub(crate) fn parse_reply(raw: &str) -> Result<String> {
    let reply: Value = serde_json::from_str(raw)
        .map_err(|error| Error::Provider(format!("invalid LM Studio response: {error}")))?;
    let output = reply
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Provider("LM Studio response has no output".into()))?;
    let text = output
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|item| item.get("content").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return Err(Error::Provider(
            "LM Studio returned no completion text".into(),
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    fn server(status: u16, body: Value) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let mut size = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    size = value.trim().parse().unwrap();
                }
            }
            let mut bytes = vec![0; size];
            reader.read_exact(&mut bytes).unwrap();
            request.push_str(std::str::from_utf8(&bytes).unwrap());
            let body = body.to_string();
            write!(stream, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            request
        });
        (base, worker)
    }

    fn fixture(
        name: &str,
        wire: Wire,
        status: u16,
        body: Value,
    ) -> (
        ProviderPreset,
        reqwest::blocking::Client,
        std::thread::JoinHandle<String>,
    ) {
        let (base, worker) = server(status, body);
        let preset = ProviderPreset {
            name: name.into(),
            wire,
            base_url: base,
            no_auth: true,
            model: String::new(),
            env_key: None,
            description: String::new(),
        };
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        (preset, client, worker)
    }

    #[test]
    fn ollama_discovers_and_caches_controls_and_supports_old_servers() {
        for (status, metadata, model, expected) in [
            (
                200,
                json!({ "thinking": { "values": [false, true] } }),
                "local",
                "none",
            ),
            (
                200,
                json!({ "thinking": { "values": ["low", "high"] } }),
                "gpt-oss",
                "low",
            ),
            (404, json!({}), "local", "none"),
        ] {
            let (preset, http, worker) = fixture("ollama", Wire::Ollama, status, metadata);
            let controls = Controls::default();
            for _ in 0..2 {
                let mut url = format!("{}/chat/completions", preset.base_url);
                let mut body = json!({ "max_tokens": 96, "stop": ["\n"] });
                assert!(!controls
                    .prepare(&http, &preset, model, None, &mut url, &mut body)
                    .unwrap());
                assert_eq!(body["reasoning_effort"], expected);
                if expected == "low" {
                    assert_eq!(body["max_tokens"], 512);
                    assert!(body.get("stop").is_none());
                }
            }
            let request = worker.join().unwrap();
            assert!(request.starts_with("POST /api/show "));
            assert!(request.contains(&format!("\"model\":\"{model}\"")));
        }
    }

    #[test]
    fn lmstudio_preserves_context_without_storage_or_integrations() {
        let (preset, http, worker) = fixture(
            "lmstudio",
            Wire::OpenAiCompat,
            200,
            json!({ "models": [{
            "key": "installed-model", "loaded_instances": [{ "id": "loaded-model" }],
            "capabilities": { "reasoning": { "allowed_options": ["off", "low"] } }
        }] }),
        );
        let mut url = String::new();
        let mut body = json!({ "messages": [
            { "role": "system", "content": "Instructions" },
            { "role": "user", "content": "CONTEXT_SENTINEL" }
        ], "max_tokens": 96, "response_format": { "type": "json_object" } });
        assert!(Controls::default()
            .prepare(&http, &preset, "loaded-model", None, &mut url, &mut body)
            .unwrap());
        assert!(url.ends_with("/api/v1/chat"));
        assert_eq!(body["reasoning"], "off");
        assert_eq!(body["input"], "CONTEXT_SENTINEL");
        assert!(body["system_prompt"]
            .as_str()
            .unwrap()
            .contains("JSON object"));
        assert_eq!(body["store"], false);
        assert_eq!(body["integrations"], json!([]));
        assert!(worker.join().unwrap().starts_with("GET /api/v1/models "));
        assert_eq!(parse_reply(r#"{"output":[{"type":"reasoning","content":"hidden"},{"type":"message","content":"git log"}]}"#).unwrap(), "git log");
    }

    #[test]
    fn invalid_metadata_and_high_only_models_fail_without_generation() {
        for metadata in [json!({ "thinking": { "values": ["high"] } }), json!(null)] {
            let (preset, http, worker) = fixture("ollama", Wire::Ollama, 200, metadata);
            let mut url = String::new();
            let mut body = json!({});
            let result =
                Controls::default().prepare(&http, &preset, "local", None, &mut url, &mut body);
            assert!(result.is_err());
            assert!(body.get("reasoning_effort").is_none());
            worker.join().unwrap();
        }
        for reply in [
            "invalid-json",
            "{}",
            r#"{"output":[{"type":"reasoning","content":"hidden"}]}"#,
        ] {
            assert!(parse_reply(reply).is_err());
        }
    }
}
