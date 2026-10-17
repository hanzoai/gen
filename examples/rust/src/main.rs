//! Hanzo Gen Rust SDK Quickstart.
//!
//! Submits a conversational turn with tool definitions to POST /v1/chat/completions.

use serde_json::json;
use std::env;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_host = env::var("HANZO_API_HOST").unwrap_or_else(|_| "https://api.hanzo.ai".into());
    let api_key = env::var("HANZO_API_KEY").unwrap_or_default();

    println!("Connecting to Gen Assistant at {api_host}...");

    let payload = json!({
        "model": "zen",
        "messages": [
            {"role": "system", "content": "You are Hanzo Gen assistant for store operations."},
            {"role": "user", "content": "Look up bin location for SKU-FRESH-APPLES"}
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "lookup_bin_location",
                    "description": "Get aisle and shelf slot for a SKU",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "sku": {"type": "string"}
                        },
                        "required": ["sku"]
                    }
                }
            }
        ],
        "tool_choice": "auto",
        "temperature": 0.1,
        "max_tokens": 512
    });

    let client = reqwest::blocking::Client::new();
    let mut req = client.post(format!("{}/v1/chat/completions", api_host.trim_end_matches('/')))
        .header("Content-Type", "application/json")
        .json(&payload);

    if !api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {api_key}"));
    }

    let resp = req.send()?;
    let status = resp.status();
    let body: serde_json::Value = resp.json()?;

    println!("HTTP Status: {status}");
    let msg = &body["choices"][0]["message"];
    if let Some(tool_calls) = msg["tool_calls"].as_array() {
        println!("Model requested tool call:\n{}", serde_json::to_string_pretty(tool_calls)?);
    } else if let Some(content) = msg["content"].as_str() {
        println!("Assistant: {}", content.trim());
    } else {
        println!("Raw response:\n{}", serde_json::to_string_pretty(&body)?);
    }

    Ok(())
}
