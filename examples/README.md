# Hanzo Gen SDK Integration Examples

Hanzo Gen (`gen`, `hanzo dev`) is the conversational assistant harness and autonomous session runner. It routes model calls through `api.hanzo.ai`, binds to external tools and Model Context Protocol (MCP) servers, maintains turn state, and streams session telemetry to Hanzo Mission Control.

## Key Capabilities

- **Multi-Turn State**: Persistent turn dialog with state serialization.
- **Tool Calling**: Native OpenAI-compatible tool/function declarations and execution loops.
- **Mission Control Streaming**: Synchronizes sessions and event streams to cloud telemetry.

## Language Examples

- **Python**: [`python/assistant.py`](python/assistant.py) (Two-turn tool dispatch and resolution)
- **TypeScript**: [`typescript/assistant.ts`](typescript/assistant.ts) (Native `fetch`, strongly typed)
- **Go**: [`go/main.go`](go/main.go) (Standard `net/http`)
- **cURL**: [`curl/assistant.sh`](curl/assistant.sh) (One-liner command line call)
- **Rust**: [`rust/src/main.rs`](rust/src/main.rs) (Using `reqwest` & `serde_json`)

## Quick Run

```bash
export HANZO_API_KEY="$(hanzo auth token)"

# Python (with live tool calling round-trip)
python3 python/assistant.py

# cURL
./curl/assistant.sh
```
