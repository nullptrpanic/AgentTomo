package main

import (
	"bufio"
	"bytes"
	"context"
	_ "embed"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"strings"
	"testing"
	"time"
)

//go:embed testdata/model-response.sse
var modelResponse string

// Run the production entrypoint as a separate process and call its official HTTP API.
func TestAgentStreamsOverHTTP(t *testing.T) {
	if port := os.Getenv("AGENTKIT_TEST_CHILD_PORT"); port != "" {
		flag.CommandLine = flag.NewFlagSet("agentkit", flag.ExitOnError)
		os.Args = []string{"agentkit", "-host", "127.0.0.1", "-port", port}
		main()
		return
	}

	ctx, cancel := context.WithTimeout(t.Context(), 20*time.Second)
	defer cancel()
	workspace := t.TempDir()
	release := make(chan struct{})
	model := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var call struct {
			Model  string
			Stream bool
			Tools  []struct {
				Function struct{ Name string }
			}
			Messages []struct {
				Role       string
				Content    json.RawMessage
				ToolCallID string `json:"tool_call_id"`
			}
		}
		if err := json.NewDecoder(r.Body).Decode(&call); err != nil {
			t.Error(err)
			return
		}
		if r.Method != "POST" || r.URL.Path != "/api/v3/chat/completions" || r.Header.Get("Authorization") != "Bearer fixture-key" ||
			call.Model != "fixture-model" || !call.Stream || len(call.Tools) != 1 || call.Tools[0].Function.Name != "exec_command" {
			t.Error("expected an authenticated streaming model call with the sandbox command tool")
			w.WriteHeader(http.StatusBadRequest)
			return
		}
		w.Header().Set("Content-Type", "text/event-stream")
		last := call.Messages[len(call.Messages)-1]
		if last.Role != "tool" {
			args, _ := json.Marshal(map[string]string{"command": "printf sandbox-tool > proof.txt; cat proof.txt"})
			chunk, _ := json.Marshal(map[string]any{
				"id": "tool-round", "object": "chat.completion.chunk", "model": "fixture-model",
				"choices": []any{map[string]any{"index": 0, "finish_reason": "tool_calls", "delta": map[string]any{
					"tool_calls": []any{map[string]any{"index": 0, "id": "fixture-exec", "type": "function",
						"function": map[string]string{"name": "exec_command", "arguments": string(args)}}},
				}}},
			})
			fmt.Fprintf(w, "data: %s\n\ndata: [DONE]\n\n", chunk)
			return
		}
		var content string
		var result struct {
			Output   string
			ExitCode int `json:"exit_code"`
		}
		if json.Unmarshal(last.Content, &content) != nil || json.Unmarshal([]byte(content), &result) != nil ||
			last.ToolCallID != "fixture-exec" || result.Output != "sandbox-tool" || result.ExitCode != 0 {
			t.Errorf("incorrect command return: %s", last.Content)
		}
		if proof, err := os.ReadFile(workspace + "/proof.txt"); err != nil || string(proof) != "sandbox-tool" {
			t.Errorf("command did not write the local artifact: %q, %v", proof, err)
		}
		first, rest, _ := strings.Cut(modelResponse, "\n\n")
		_, _ = io.WriteString(w, first+"\n\n")
		w.(http.Flusher).Flush()
		select {
		case <-release:
			_, _ = io.WriteString(w, rest)
		case <-r.Context().Done():
		case <-ctx.Done():
		}
	}))
	defer model.Close()

	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	address := listener.Addr().String()
	port := fmt.Sprint(listener.Addr().(*net.TCPAddr).Port)
	if err := listener.Close(); err != nil {
		t.Fatal(err)
	}
	child := exec.CommandContext(ctx, os.Args[0], "-test.run=^TestAgentStreamsOverHTTP$")
	child.Dir = workspace
	child.Env = append(os.Environ(), "AGENTKIT_TEST_CHILD_PORT="+port, "MODEL_AGENT_PROVIDER=ark", "MODEL_AGENT_NAME=fixture-model",
		"MODEL_AGENT_API_BASE="+model.URL+"/api/v3", "MODEL_AGENT_API_KEY=fixture-key")
	var logs bytes.Buffer
	child.Stdout, child.Stderr = &logs, &logs
	if err := child.Start(); err != nil {
		t.Fatal(err)
	}
	exited := make(chan struct{})
	go func() { _ = child.Wait(); close(exited) }()
	defer func() { cancel(); <-exited }()

	post := func(path, body string) (*http.Response, error) {
		request, err := http.NewRequestWithContext(ctx, "POST", "http://"+address+path, strings.NewReader(body))
		if err != nil {
			return nil, err
		}
		request.Header.Set("Content-Type", "application/json")
		return http.DefaultClient.Do(request)
	}
	for {
		response, err := post("/apps/agentkit/users/local/sessions/test", "{}")
		if err == nil {
			_ = response.Body.Close()
			if response.StatusCode != http.StatusOK {
				t.Fatalf("create session: HTTP %d", response.StatusCode)
			}
			break
		}
		select {
		case <-exited:
			t.Fatalf("Agent exited before accepting an HTTP session: %s", &logs)
		case <-ctx.Done():
			t.Fatal(ctx.Err())
		case <-time.After(20 * time.Millisecond):
		}
	}
	response, err := post("/run_sse", `{"appName":"agentkit","userId":"local","sessionId":"test","newMessage":{"role":"user","parts":[{"text":"记住青竹"}]},"streaming":true}`)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK || !strings.HasPrefix(response.Header.Get("Content-Type"), "text/event-stream") {
		t.Fatalf("run_sse: %s, %s", response.Status, response.Header.Get("Content-Type"))
	}
	var partial, final string
	lines := bufio.NewScanner(response.Body)
	for lines.Scan() {
		data, ok := strings.CutPrefix(lines.Text(), "data: ")
		if !ok {
			continue
		}
		var event struct {
			Partial bool
			Error   string
			Content struct {
				Parts []struct {
					Text    string
					Thought bool
				}
			}
		}
		if err := json.Unmarshal([]byte(data), &event); err != nil || event.Error != "" {
			t.Fatalf("invalid agent event: %s (%v)", data, err)
		}
		for _, part := range event.Content.Parts {
			if part.Thought || part.Text == "" {
				continue
			}
			if event.Partial {
				partial += part.Text
				if partial == "已记住。" {
					close(release)
				}
			} else {
				final += part.Text
			}
		}
	}
	if lines.Err() != nil || partial != "已记住。\n青竹" || final != partial {
		t.Fatalf("incomplete stream: partial %q, final %q, error %v", partial, final, lines.Err())
	}
}
