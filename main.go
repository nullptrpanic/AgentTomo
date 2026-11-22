package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"strings"

	veagent "github.com/volcengine/veadk-go/v2/agent/llmagent"
	"github.com/volcengine/veadk-go/v2/runner"
	ark "github.com/volcengine/volcengine-go-sdk/service/arkruntime/model"
	"google.golang.org/adk/v2/agent"
)

func main() {
	slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, nil)))
	if len(os.Args) != 1 {
		fmt.Fprintln(os.Stderr, "usage: agenttomo-agentkit < prompts.jsonl")
		os.Exit(2)
	}
	if err := chat(context.Background(), os.Stdin, os.Stdout); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func chat(ctx context.Context, input io.Reader, output io.Writer) error {
	cfg := &veagent.Config{
		ModelProvider: "ark",
		ModelName:     os.Getenv("MODEL_AGENT_NAME"),
		ModelAPIBase:  os.Getenv("MODEL_AGENT_API_BASE"),
		ModelAPIKey:   os.Getenv("MODEL_AGENT_API_KEY"),
	}
	if cfg.ModelName == "" || cfg.ModelAPIBase == "" || cfg.ModelAPIKey == "" {
		return errors.New("set MODEL_AGENT_NAME, MODEL_AGENT_API_BASE and MODEL_AGENT_API_KEY")
	}
	cfg.Name = "agentkit"
	cfg.Description = "A text conversation assistant."
	cfg.Instruction = "用用户使用的语言回答问题。"
	root, err := veagent.New(cfg)
	if err != nil {
		return err
	}
	r, err := runner.New(runner.Config{Agent: root, AutoCreateSession: true})
	if err != nil {
		return err
	}

	in, out := json.NewDecoder(input), json.NewEncoder(output)
	in.DisallowUnknownFields()
	runConfig := agent.RunConfig{StreamingMode: agent.StreamingModeSSE}
	for {
		var turn struct{ Prompt string }
		err := in.Decode(&turn)
		if err == io.EOF {
			return nil
		}
		if err != nil || strings.TrimSpace(turn.Prompt) == "" {
			return errors.New("expected a JSON object with a nonempty prompt")
		}
		var reply strings.Builder
		for event, err := range r.Run(ctx, "local", "chat", runner.UserText(turn.Prompt), runConfig) {
			if err != nil {
				var failure *ark.APIError
				if errors.As(err, &failure) && failure.HTTPStatusCode > 0 {
					return fmt.Errorf("model request failed (HTTP %d)", failure.HTTPStatusCode)
				}
				return errors.New("model request failed")
			}
			if event == nil || event.Content == nil {
				continue
			}
			for _, part := range event.Content.Parts {
				if part == nil || part.Thought || part.Text == "" {
					continue
				}
				if !event.Partial {
					reply.WriteString(part.Text)
					continue
				}
				if err := out.Encode(map[string]string{"delta": part.Text}); err != nil {
					return err
				}
			}
		}
		if strings.TrimSpace(reply.String()) == "" {
			return errors.New("model returned no text")
		}
		if err := out.Encode(map[string]string{"reply": reply.String()}); err != nil {
			return err
		}
	}
}
