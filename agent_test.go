package main

import (
	"bufio"
	"bytes"
	"context"
	_ "embed"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"slices"
	"strings"
	"testing"
	"time"
)

//go:embed testdata/model-response.sse
var modelResponse string

const firstDelta = `{"delta":"已记住。"}` + "\n"
const turnOutput = firstDelta + `{"delta":"\n"}` + "\n" + `{"delta":"青竹"}` + "\n" + `{"reply":"已记住。\n青竹"}` + "\n"

func useModel(t *testing.T, handler http.HandlerFunc) {
	t.Helper()
	server := httptest.NewServer(handler)
	t.Cleanup(server.Close)
	t.Setenv("MODEL_AGENT_NAME", "fixture-model")
	t.Setenv("MODEL_AGENT_API_BASE", server.URL+"/api/v3")
	t.Setenv("MODEL_AGENT_API_KEY", "fixture-key")
}

func TestChat(t *testing.T) {
	type message struct{ Role, Content string }
	calls := make(chan []message, 8)
	useModel(t, func(w http.ResponseWriter, r *http.Request) {
		var req struct {
			Model    string
			Messages []message
			Tools    []json.RawMessage
			Stream   bool
		}
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			t.Error(err)
			http.Error(w, "invalid request", http.StatusBadRequest)
			return
		}
		if r.Method != "POST" || r.URL.Path != "/api/v3/chat/completions" || r.Header.Get("Authorization") != "Bearer fixture-key" {
			t.Error("wrong model route or authorization")
		}
		if req.Model != "fixture-model" || !req.Stream || len(req.Tools) != 0 {
			t.Error("expected a streaming text request")
		}
		dialog := slices.DeleteFunc(req.Messages, func(m message) bool { return m.Role == "system" })
		if len(dialog) == 0 {
			t.Error("missing user message")
			w.WriteHeader(http.StatusBadRequest)
			return
		}
		calls <- dialog
		w.Header().Set("Content-Type", "text/event-stream")
		switch dialog[len(dialog)-1].Content {
		case "reject":
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(http.StatusUnauthorized)
			_, _ = w.Write([]byte(`{"error":{"message":"invalid fixture-key","code":"InvalidAPIKey"}}`))
		case "broken stream":
			first, _, _ := strings.Cut(modelResponse, "\n\n")
			_, _ = io.WriteString(w, first+"\n\n"+`data: {"error":{"code":"InternalError","message":"fixture-key"}}`+"\n\n")
		case "empty":
			_, _ = io.WriteString(w, "data: [DONE]\n\n")
		default:
			_, _ = io.WriteString(w, modelResponse)
		}
	})

	first := `{"prompt":"记住代号青竹"}`
	next := `{"prompt":"代号是什么？\n请简答"}`
	const badPrompt = "expected a JSON object with a nonempty prompt"
	for _, tc := range []struct{ input, output, problem string }{
		{first + "\n" + next, turnOutput + turnOutput, ""},
		{next, turnOutput, ""},
		{`{"prompt":" "}`, "", badPrompt},
		{`{"prompt":`, "", badPrompt},
		{`{"prompt":"reject"}`, "", "model request failed (HTTP 401)"},
		{`{"prompt":"broken stream"}`, firstDelta, "model request failed"},
		{`{"prompt":"empty"}`, "", "model returned no text"},
	} {
		var out bytes.Buffer
		err := chat(t.Context(), strings.NewReader(tc.input), &out)
		if out.String() != tc.output || (err == nil) != (tc.problem == "") {
			t.Fatalf("input %q: output %q, error %v", tc.input, &out, err)
		}
		if err != nil && err.Error() != tc.problem {
			t.Fatalf("input %q: unexpected diagnostic %v", tc.input, err)
		}
	}
	firstUser := message{"user", "记住代号青竹"}
	nextUser := message{"user", "代号是什么？\n请简答"}
	want := [][]message{
		{firstUser},
		{firstUser, {"assistant", "已记住。\n青竹"}, nextUser},
		{nextUser},
		{{"user", "reject"}},
		{{"user", "broken stream"}},
		{{"user", "empty"}},
	}
	if len(calls) != len(want) {
		t.Fatalf("got %d model requests, want %d", len(calls), len(want))
	}
	for _, history := range want {
		if got := <-calls; !slices.Equal(got, history) {
			t.Fatalf("model saw %v, want %v", got, history)
		}
	}
}

func TestFirstDeltaArrivesBeforeCompletion(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 5*time.Second)
	defer cancel()
	release := make(chan struct{})
	useModel(t, func(w http.ResponseWriter, r *http.Request) {
		first, rest, _ := strings.Cut(modelResponse, "\n\n")
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, `data: {"choices":[{"index":0,"delta":{"reasoning_content":"private fixture text"}}]}`+"\n\n")
		_, _ = io.WriteString(w, first+"\n\n")
		w.(http.Flusher).Flush()
		select {
		case <-release:
			_, _ = io.WriteString(w, rest)
		case <-r.Context().Done():
		}
	})

	reader, writer := io.Pipe()
	defer reader.Close()
	go func() {
		_ = writer.CloseWithError(chat(ctx, strings.NewReader(`{"prompt":"你好"}`), writer))
	}()
	lines := bufio.NewReader(reader)
	first, err := lines.ReadString('\n')
	if err != nil || first != firstDelta {
		t.Fatalf("before model completion: got %q, error %v", first, err)
	}
	close(release)
	rest, err := io.ReadAll(lines)
	if err != nil || first+string(rest) != turnOutput {
		t.Fatalf("completed stream: got %q, error %v", first+string(rest), err)
	}
}
