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
	"sync"
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

func commandEnv(environ []string) []string {
	env := make([]string, 0, 16)
	for _, entry := range environ {
		name, _, _ := strings.Cut(entry, "=")
		switch name {
		case "PATH", "HOME", "USER", "LOGNAME", "TERM", "SHELL",
			"LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "TZ", "PWD", "SHLVL",
			"SSL_CERT_FILE", "SSL_CERT_DIR", "REQUESTS_CA_BUNDLE",
			"NODE_EXTRA_CA_CERTS", "CURL_CA_BUNDLE",
			"AGENTTOMO_AGENT_ID", "AGENTTOMO_TASK_ID", "AGENTTOMO_TOOLCALL_ID":
			env = append(env, entry)
		}
	}
	return env
}

func runCommand(ctx agent.Context, args commandArgs) (map[string]any, error) {
	return runShellCommand(ctx, args.Command, time.Minute)
}

func runShellCommand(ctx context.Context, command string, limit time.Duration) (map[string]any, error) {
	ctxWithTimeout, cancel := context.WithTimeout(ctx, limit)
	defer cancel()
	cmd := exec.Command("/bin/bash", "-c", command)
	cmd.SysProcAttr = &unix.SysProcAttr{Setpgid: true}
	cmd.Env = commandEnv(os.Environ())
	cmd.WaitDelay = time.Second
	var output commandOutput
	cmd.Stdout, cmd.Stderr = &output, &output
	err := ctxWithTimeout.Err()
	if err == nil {
		err = cmd.Start()
	}
	if err == nil {
		pgid := cmd.Process.Pid
		var mu sync.Mutex
		reaped := false
		timedOut := false
		done := make(chan struct{})
		watchDone := make(chan struct{})
		go func() {
			defer close(watchDone)
			select {
			case <-ctxWithTimeout.Done():
				mu.Lock()
				if !reaped {
					timedOut = true
					_ = unix.Kill(-pgid, unix.SIGKILL)
				}
				mu.Unlock()
			case <-done:
			}
		}()
		// Observe exit without reaping the group leader. Its PID cannot be
		// reused while we stop background children and release their pipes.
		var info unix.Siginfo
		var waitErr error
		for {
			waitErr = unix.Waitid(unix.P_PID, pgid, &info, unix.WEXITED|unix.WNOWAIT, nil)
			if !errors.Is(waitErr, unix.EINTR) {
				break
			}
		}
		mu.Lock()
		_ = unix.Kill(-pgid, unix.SIGKILL)
		reaped = true
		wasTimedOut := timedOut
		mu.Unlock()
		close(done)
		<-watchDone
		waitCmdErr := cmd.Wait()
		if wasTimedOut && ctxWithTimeout.Err() != nil {
			err = errors.Join(waitErr, waitCmdErr, ctxWithTimeout.Err())
		} else {
			err = errors.Join(waitErr, waitCmdErr)
		}
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
