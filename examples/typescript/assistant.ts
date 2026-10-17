/**
 * Hanzo Gen TypeScript SDK Quickstart.
 *
 * Demonstrates multi-turn conversational assistant with tool calling.
 */

const API_HOST = process.env.HANZO_API_HOST || "https://api.hanzo.ai";
const API_KEY = process.env.HANZO_API_KEY || "";

interface ToolFunction {
  name: string;
  description: string;
  parameters: Record<string, any>;
}

async function runAssistantTurn(messages: any[], tools?: Array<{ type: string; function: ToolFunction }>) {
  const url = `${API_HOST.replace(/\/$/, "")}/v1/chat/completions`;
  const response = await fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      ...(API_KEY ? { Authorization: `Bearer ${API_KEY}` } : {}),
    },
    body: JSON.stringify({
      model: "zen",
      messages,
      tools,
      tool_choice: tools ? "auto" : undefined,
      max_tokens: 512,
      temperature: 0.1,
    }),
  });

  if (!response.ok) {
    throw new Error(`HTTP ${response.status}: ${await response.text()}`);
  }

  const data: any = await response.json();
  return data.choices[0]?.message;
}

async function main() {
  console.log(`Connecting to Hanzo Gen Assistant at ${API_HOST}...`);

  const tools = [
    {
      type: "function",
      function: {
        name: "get_truck_eta",
        description: "Fetch real-time GPS arrival estimate for a logistics carrier",
        parameters: {
          type: "object",
          properties: {
            carrier_id: { type: "string" },
            destination_dock: { type: "string" },
          },
          required: ["carrier_id", "destination_dock"],
        },
      },
    },
  ];

  const messages: any[] = [
    { role: "system", content: "You are Hanzo Gen, an enterprise logistics assistant. Call get_truck_eta when asked about delivery times." },
    { role: "user", content: "What is the ETA for carrier TRUCK-889 arriving at dock D-04?" },
  ];

  console.log("\n[Turn 1] Sending user query...");
  const reply = await runAssistantTurn(messages, tools);

  if (reply.tool_calls && reply.tool_calls.length > 0) {
    const call = reply.tool_calls[0];
    console.log(`\nTool requested: ${call.function.name}(${call.function.arguments})`);
    
    // Mock tool response
    const toolResult = { eta_minutes: 24, status: "in_transit", temperature_celsius: -18.2 };
    
    messages.push(reply);
    messages.push({
      role: "tool",
      tool_call_id: call.id,
      content: JSON.stringify(toolResult),
    });

    console.log("\n[Turn 2] Resolving final response...");
    const finalReply = await runAssistantTurn(messages);
    console.log(`Assistant: ${finalReply.content || finalReply.reasoning_content}`);
  } else {
    console.log(`Assistant: ${reply.content || reply.reasoning_content}`);
  }
}

main().catch(console.error);
