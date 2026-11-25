// Copyright (c) 2025 Beijing Volcano Engine Technology Co., Ltd. and/or its affiliates.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

package main

import (
	"bytes"
	"context"
	"errors"
	"os"
	"os/exec"
	"strings"
	"time"

	"golang.org/x/sys/unix"

	veagent "github.com/volcengine/veadk-go/v2/agent/llmagent"
	"google.golang.org/adk/v2/agent"
	"google.golang.org/adk/v2/agent/llmagent"
	"google.golang.org/adk/v2/tool"
	"google.golang.org/adk/v2/tool/functiontool"
)

func buildSampleAgent() (agent.Agent, error) {
	command, err := functiontool.New(functiontool.Config{
		Name:        "exec_command",
		Description: "Run a shell command in the current sandbox. Use it to execute programs and read or write files. Returns output and exit_code.",
	}, runCommand)
	if err != nil {
		return nil, err
	}
	agentName := "agentkit"
	var description string
	description = `A task assistant with a sandbox command tool.`

	var instruction string
	instruction = `用用户使用的语言回答问题。`

	cfg := &veagent.Config{
		Config: llmagent.Config{
			Name:        agentName,
			Description: description,
			Instruction: instruction,
			Tools:       []tool.Tool{command},
		},
		ModelExtraConfig: map[string]any{
			"extra_body": map[string]any{
				"thinking": map[string]string{
					"type": "disabled",
				},
			},
		},
	}
	return veagent.New(cfg)
}

type commandArgs struct {
	Command string `json:"command" jsonschema:"Shell command to execute in the current sandbox."`
}

const commandOutputLimit = 1 << 20

type commandOutput struct {
	buffer    bytes.Buffer
	truncated bool
}

func (b *commandOutput) Write(data []byte) (int, error) {
	keep := min(len(data), commandOutputLimit-b.buffer.Len())
	b.buffer.Write(data[:keep])
	b.truncated = b.truncated || keep < len(data)
	return len(data), nil
}

func runCommand(ctx agent.Context, args commandArgs) (map[string]any, error) {
	ctxWithTimeout, cancel := context.WithTimeout(ctx, time.Minute)
	defer cancel()
	cmd := exec.CommandContext(ctxWithTimeout, "/bin/bash", "-c", args.Command)
	cmd.SysProcAttr = &unix.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error {
		err := unix.Kill(-cmd.Process.Pid, unix.SIGKILL)
		if errors.Is(err, unix.ESRCH) {
			return os.ErrProcessDone
		}
		return err
	}
	cmd.Env = []string{}
	for _, entry := range os.Environ() {
		name, _, _ := strings.Cut(entry, "=")
		switch name {
		case "MODEL_AGENT_API_KEY", "OPENAI_API_KEY", "ARK_API_KEY":
			continue
		}
		cmd.Env = append(cmd.Env, entry)
	}
	cmd.WaitDelay = time.Second
	var output commandOutput
	cmd.Stdout, cmd.Stderr = &output, &output
	err := cmd.Start()
	if err == nil {
		// Observe exit without reaping the group leader. Its PID cannot be
		// reused while we stop background children and release their pipes.
		var info unix.Siginfo
		var waitErr error
		for {
			waitErr = unix.Waitid(unix.P_PID, cmd.Process.Pid, &info, unix.WEXITED|unix.WNOWAIT, nil)
			if !errors.Is(waitErr, unix.EINTR) {
				break
			}
		}
		_ = cmd.Cancel()
		err = errors.Join(waitErr, cmd.Wait())
	}
	result := map[string]any{"output": output.buffer.String(), "exit_code": -1}
	if output.truncated {
		result["output_truncated"] = true
	}
	if cmd.ProcessState != nil {
		result["exit_code"] = cmd.ProcessState.ExitCode()
	}
	if err != nil {
		result["error"] = err.Error()
	}
	return result, nil
}
