#!/usr/bin/env python3
"""Hanzo Gen Conversational Assistant Python Quickstart.

Demonstrates stateful multi-turn assistant session with tool dispatch:
1. Maintain structured conversation turns
2. Declare tool/function interfaces (OpenAI-compatible)
3. Execute local tool calls and feed results back to the model
"""

import json
import os
import urllib.request

API_HOST = os.getenv("HANZO_API_HOST", "https://api.hanzo.ai")
API_KEY = os.getenv("HANZO_API_KEY", "")


def get_headers():
    return {
        "Content-Type": "application/json",
        "Authorization": f"Bearer {API_KEY}" if API_KEY else "",
    }


def call_assistant(messages: list, tools: list = None) -> dict:
    url = f"{API_HOST.rstrip('/')}/v1/chat/completions"
    payload = {
        "model": "zen",
        "messages": messages,
        "max_tokens": 512,
        "temperature": 0.1,
    }
    if tools:
        payload["tools"] = tools
        payload["tool_choice"] = "auto"

    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode("utf-8"),
        headers=get_headers(),
        method="POST"
    )
    with urllib.request.urlopen(req) as resp:
        return json.loads(resp.read().decode("utf-8"))


# Mock tool implementation
def query_warehouse_inventory(sku: str) -> dict:
    print(f"  [Tool Executed: query_warehouse_inventory(sku='{sku}')]")
    return {"sku": sku, "on_hand": 142, "bin_location": "B-04-R2", "reserved": 12}


def main():
    print(f"Initializing Hanzo Gen Assistant on {API_HOST}...")

    tools = [
        {
            "type": "function",
            "function": {
                "name": "query_warehouse_inventory",
                "description": "Look up inventory counts and bin location for a specific SKU",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "sku": {"type": "string", "description": "The SKU code (e.g. SKU-ORGANIC-MILK-1G)"}
                    },
                    "required": ["sku"]
                }
            }
        }
    ]

    messages = [
        {"role": "system", "content": "You are Hanzo Gen, an enterprise logistics assistant. When a user asks about stock, use query_warehouse_inventory."},
        {"role": "user", "content": "How many units of SKU-ORGANIC-MILK-1G do we have in the warehouse, and which bin is it in?"}
    ]

    print("\n[Turn 1] User: How many units of SKU-ORGANIC-MILK-1G do we have?")
    resp = call_assistant(messages, tools=tools)
    msg = resp["choices"][0]["message"]

    # Check for tool calls
    if "tool_calls" in msg and msg["tool_calls"]:
        tool_call = msg["tool_calls"][0]
        func_name = tool_call["function"]["name"]
        args = json.loads(tool_call["function"]["arguments"])
        
        print(f"\n[Model Requested Tool Call]: {func_name}")
        tool_result = query_warehouse_inventory(**args)

        # Append assistant turn and tool turn
        messages.append(msg)
        messages.append({
            "role": "tool",
            "tool_call_id": tool_call["id"],
            "content": json.dumps(tool_result)
        })

        # Turn 2: Assistant responds with tool output
        print("\n[Turn 2] Resolving final assistant response with tool result...")
        final_resp = call_assistant(messages)
        final_msg = final_resp["choices"][0]["message"]
        content = final_msg.get("content") or final_msg.get("reasoning_content") or ""
        print(f"\nAssistant:\n{content.strip()}")
    else:
        content = msg.get("content") or msg.get("reasoning_content") or ""
        print(f"\nAssistant:\n{content.strip()}")


if __name__ == "__main__":
    main()
