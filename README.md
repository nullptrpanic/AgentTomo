# AgentTomo 的 AgentKit 对话循环

每个进程运行一个火山引擎 VeADK Go Runner，同一进程共用内存会话。标准输入每轮接收一个 JSON 对象，如 `{"prompt":"你好"}`。Agent 调用方舟 Chat Completions 时固定发送 `stream: true`，逐段读取 SSE。[方舟对话 API](https://docs.volcengine.com/docs/ark/chat-api?lang=zh)

标准输出每行一个 JSON 对象。调用方收到 `{"delta":"你"}`、`{"delta":"好"}` 时依次追加文字，收到 `{"reply":"你好"}` 时用完整回复确认本轮结束。关闭标准输入即可正常退出。输入或模型错误会写入标准错误并以非零状态退出，此时已收到的片段是未完成的回复。

运行环境提供 `MODEL_AGENT_NAME`（模型或推理接入点 ID）、`MODEL_AGENT_API_BASE`（方舟 API 基础地址）和 `MODEL_AGENT_API_KEY`（模型凭据）。接入 Playground 时，占位凭据和宿主转发由宿主负责。

```bash
export GOROOT=/opt/go
export GOPATH=/home/openclaw/code/go
"$GOROOT/bin/go" test -count=1 ./...
CGO_ENABLED=0 "$GOROOT/bin/go" build -ldflags='-s -w' -o dist/agenttomo-agentkit .
printf '%s\n' '{"prompt":"记住代号青竹"}' '{"prompt":"代号是什么？"}' | ./dist/agenttomo-agentkit
```

自动测试使用回环模型端点核对流式请求、首段到达时机和多轮历史。最后一条命令使用运行环境指定的模型。
