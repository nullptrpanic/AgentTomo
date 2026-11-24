# AgentTomo 的火山 AgentKit Go 服务

项目从火山 AgentKit CLI 0.9.2 的 `basic_go` 模板生成。Go 入口使用 VeADK Go v2 的 `AgentkitServerApp`，由 SDK 管理会话、工具循环和 `POST /run_sse` 流式输出。Agent 通过官方 `functiontool` 注册一个 `exec_command` 工具，用于在当前沙箱执行命令和读写文件，返回输出与退出码。单次命令最长运行 60 秒。Playground 在每个沙箱内启动回环 HTTP 服务，宿主负责转发模型凭据。[官方 Go 模板](https://github.com/volcengine/agentkit-sdk-python/blob/v0.9.2/agentkit/toolkit/resources/samples/veadk_go_basic/main.go) · [官方 Go 服务](https://github.com/volcengine/veadk-go/blob/v2.0.0/examples/apps/agentkit_server_app/main.go)

生成原始模板时，在空目录运行以下命令。项目将模板的服务入口换成了支持 SSE 的官方 v2 入口。`agentkit.yaml` 使用本地模式。CLI 0.9.2 的 `language_version` 只接受 Go 1.24，实际构建镜像通过 `docker_build.base_image.builder` 指定为 Go 1.26.5，与 `go.mod` 的最低版本一致。[CLI 配置定义](https://github.com/volcengine/agentkit-sdk-python/blob/v0.9.2/agentkit/toolkit/config/config.py) · [构建镜像配置](https://volcengine.github.io/agentkit-sdk-python/content/2.agentkit-cli/3.configurations.html#base-image)

```bash
agentkit init agentkit --template basic_go --agent-name agentkit \
  --description 'A text conversation assistant.' --system-prompt '用用户使用的语言回答问题。'
```

## 构建与测试

在本目录运行 `make` 即可构建，产物为 `dist/agenttomo-agentkit`。从 AgentTomo 仓库根目录构建可运行 `make -C example/agent/agentkit`。

从仓库根目录运行 Go 测试。

```bash
export GOROOT=/opt/go
export GOPATH=/home/openclaw/code/go
"$GOROOT/bin/go" -C example/agent/agentkit test -count=1 ./...
```

在仓库根目录运行 `./playground -c`，宿主从 `config.json` 的 AgentKit 条目读取程序路径、模型接入点和凭据。沙箱中的 SDK 接收会话占位凭据，模型调用经过宿主代理。单独运行 Go 服务时，模型调用权限由运行环境提供。自动测试使用本地模拟模型。[Playground 配置](../../../crates/agenttomo-box/README.md#从配置文件启动)

## 本地调用

准备好模型配置后，在两个终端里分别启动服务和发送请求。

```bash
example/agent/agentkit/dist/agenttomo-agentkit -host 127.0.0.1 -port 8000
```

```bash
curl --fail -X POST http://127.0.0.1:8000/apps/agentkit/users/local/sessions/demo
curl --fail --no-buffer http://127.0.0.1:8000/run_sse \
  -H 'Content-Type: application/json' \
  -d '{"appName":"agentkit","userId":"local","sessionId":"demo","streaming":true,"newMessage":{"role":"user","parts":[{"text":"你好"}]}}'
```

后续提示复用同一个 `sessionId`。模型输出在出站 Chat Completions 请求中使用 SSE，Agent 的 HTTP 回复采用 ADK 事件格式。Playground 的 Stop 会终止该沙箱里的 Agent 服务，下一轮重新创建会话。

Security tests 的三个样例与 Codex 共用准备、注入和产物检查流程。每次 Run 启动独立的 AgentKit 进程和 SDK 会话，结束或停止时清理该进程，并恢复普通 Chat 的会话。工具在 AgentTomo 沙箱内运行，模型侧的工具调用与返回由流式协议解码器记录，进程和文件活动继续由原有采集器记录。
