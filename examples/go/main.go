// Hanzo Gen Go Quickstart.
// Demonstrates conversational turns with model tool calling.
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"strings"
)

type ChatRequest struct {
	Model       string    `json:"model"`
	Messages    []Message `json:"messages"`
	Tools       []Tool    `json:"tools,omitempty"`
	MaxTokens   int       `json:"max_tokens"`
	Temperature float64   `json:"temperature"`
}

type Message struct {
	Role       string     `json:"role"`
	Content    string     `json:"content"`
	ToolCalls  []ToolCall `json:"tool_calls,omitempty"`
	ToolCallID string     `json:"tool_call_id,omitempty"`
}

type Tool struct {
	Type     string       `json:"type"`
	Function ToolFunction `json:"function"`
}

type ToolFunction struct {
	Name        string                 `json:"name"`
	Description string                 `json:"description"`
	Parameters  map[string]interface{} `json:"parameters"`
}

type ToolCall struct {
	ID       string `json:"id"`
	Type     string `json:"type"`
	Function struct {
		Name      string `json:"name"`
		Arguments string `json:"arguments"`
	} `json:"function"`
}

type ChatResponse struct {
	Choices []struct {
		Message Message `json:"message"`
	} `json:"choices"`
}

func main() {
	apiHost := os.Getenv("HANZO_API_HOST")
	if apiHost == "" {
		apiHost = "https://api.hanzo.ai"
	}
	apiKey := os.Getenv("HANZO_API_KEY")

	fmt.Printf("Connecting to Hanzo Gen Assistant at %s...\n", apiHost)

	tools := []Tool{
		{
			Type: "function",
			Function: ToolFunction{
				Name:        "get_store_schedule",
				Description: "Get operating hours for a store number",
				Parameters: map[string]interface{}{
					"type": "object",
					"properties": map[string]interface{}{
						"store_id": map[string]string{"type": "string"},
					},
					"required": []string{"store_id"},
				},
			},
		},
	}

	messages := []Message{
		{Role: "system", Content: "You are Hanzo Gen assistant. Call get_store_schedule when asked about hours."},
		{Role: "user", Content: "What time does store #4615 open tomorrow?"},
	}

	reqBody := ChatRequest{
		Model:       "zen",
		Messages:    messages,
		Tools:       tools,
		MaxTokens:   512,
		Temperature: 0.1,
	}

	jsonBytes, _ := json.Marshal(reqBody)
	url := fmt.Sprintf("%s/v1/chat/completions", strings.TrimRight(apiHost, "/"))
	req, _ := http.NewRequest("POST", url, bytes.NewBuffer(jsonBytes))
	req.Header.Set("Content-Type", "application/json")
	if apiKey != "" {
		req.Header.Set("Authorization", fmt.Sprintf("Bearer %s", apiKey))
	}

	client := &http.Client{}
	resp, err := client.Do(req)
	if err != nil {
		panic(err)
	}
	defer resp.Body.Close()

	bodyBytes, _ := io.ReadAll(resp.Body)
	var chatResp ChatResponse
	_ = json.Unmarshal(bodyBytes, &chatResp)

	if len(chatResp.Choices) > 0 {
		msg := chatResp.Choices[0].Message
		if len(msg.ToolCalls) > 0 {
			tc := msg.ToolCalls[0]
			fmt.Printf("Model requested tool call: %s with args %s\n", tc.Function.Name, tc.Function.Arguments)
		} else {
			fmt.Printf("Assistant: %s\n", msg.Content)
		}
	}
}
