# AgentTomo Playground

## 目标与范围

提供两个公开子命令：`agenttomo-box run -c '…'` 保留当前受管执行语义；`agenttomo-box playground` 启动 Web 服务。网页左侧是 Docker 容器中的真实终端，右侧实时展示该会话的进程 trace。

当前 trace 范围是 fork/vfork/clone、成功 exec、退出、文件打开、mmap 调用、TCP 转发和 UDP 收发/转发。默认 HTTPS 保持密文；开启 `--capture-http` 后，通过 OverlayFS 注入任务 CA，支持 HTTP/1.1 请求/响应内容与 TLS 解密，在 Traffic 页展示真实头信息和限量正文，原事件 JSON 详情保留。不读取 Agent 私有会话文件，不增加 Agent 语义 trace 关联。支持边界见[内容检查设计](content-inspection-design.md)、[文件与 TCP 采集](file-network-collection.md)、[mmap 采集](mmap-collection.md)和[UDP 采集](udp-collection.md)。网络事件只由代理生成；TCP 关联建连者，UDP 保留流参与者，多参与者时不猜单个 PID。

## 架构判断与方案

现有前台、采集 worker、专用 ptrace 线程的分工合理：前台寿命和目标后代寿命分离，ptrace 操作固定在同一个 OS 线程，落盘与异步 I/O 由 worker 负责。需要补足的是交互终端传输，而不是重写采集器。

本次选择 Axum WebSocket + 本地打包的 xterm.js；Docker 使用现有 CLI。独立 WebTTY 服务会引入第二套会话和 trace 通道管理；把 Docker/Web 代码塞进 tracer 会混淆职责。直接采用平台 PTY（通过 nix）补足 worker 的终端模式，复用原采集链路。

```mermaid
flowchart LR
    Browser[浏览器：Terminal / Trace] <-->|WebSocket| Web[playground 服务]
    Web --> Docker[本地 Docker CLI]
    subgraph Container[每个页面一个临时容器]
        Worker[采集 worker]
        PTY[PTY]
        Bash[交互 Bash]
        Trace[ptrace 线程]
        Worker <--> PTY
        PTY <--> Bash
        Trace --> Bash
        Trace --> Worker
    end
    Docker <-->|私有 IPC| Worker
    Worker --> Logs[会话日志目录]
    Logs -->|追加事件| Web
```

## 模块边界

- `cli/`：Clap 子命令与参数；`main.rs` 仅分派入口和管理 runtime。
- `execution/`：前台、采集 worker、启动屏障和私有 IPC。
- `tracing/`：原生进程事件、ptrace 停点与恢复、exec 元数据。
- `syscall/`：选择性 seccomp 过滤、目标内存和寄存器机制。
- `collectors/file/`、`collectors/memory/`、`collectors/network/`：分别解释文件、内存映射与网络 syscall，互不引用。
- `proxy/tcp/`、`proxy/udp/`：同一 worker 内相互独立的 Tokio 转发，不依赖 ptrace 或文件采集。
- `terminal/`：PTY 分配、异步读写与窗口尺寸；不包含 Docker 或 Web 逻辑。
- `recording/`：原始输出和 JSONL 证据、根退出与采集完成状态。
- `playground/`：HTTP/WS、Docker 容器会话、日志追加读取、静态页面。

依赖方向是 playground 通过 worker 的 IPC 与证据文件使用采集能力；execution 启动 tracing 与 proxy，并管理 terminal、recording。底层模块不依赖 playground；tracing 分发 syscall 给各采集模块，network 只将私有五元组/进程元数据交给关联表，代理使用内核实际接收的地址查表。namespace 与常驻 socket 的初始化独立放在 `network/`。

## 会话与终端

默认每个页面建立一个独立临时容器，页面断开或服务正常停止时删除该容器，日志保留。Docker 先 create，再 attach/start，以便连接失败时也能准确清理自己的容器。仅删除当前会话创建的容器，不执行全局 prune。

Docker CLI 子进程使用独立进程组，宿主终端的 Ctrl-C 由 Playground 处理，连续按键不会打断创建、attach 或清理命令；网页终端中的 Ctrl-C 仍通过 PTY 作用于目标前台任务。控制命令保留 30 秒超时。`docker rm` 失败或超时时，再查询该名称的容器：只有查询成功且确认不存在才把清理记为成功；容器仍存在或无法查询时，记录原命令的退出码/信号或 I/O 错误，以及复核结果，不把 Docker 权限/连接错误视为容器已删除。

默认使用本地 `agenttomo-playground:24.04`，基于 Ubuntu 24.04 预装网络、进程和文件诊断工具；构建方法见[镜像说明](../docker/playground/README.md)。可由 `--image` 指定其他兼容镜像，不自动拉取或构建。只挂载只读的 box 二进制和该会话日志目录；不挂载项目、宿主根目录或 Docker socket 到目标容器。目标保持当前用户 UID/GID、cap-drop=ALL 和 no-new-privileges，不使用 privileged。为允许嵌套 user/net namespace，开发容器设置 seccomp=unconfined、apparmor=unconfined；这放宽了容器策略。网络工具与内核 TPROXY 能力不足时启动失败，不回退旧路径。

终端使用独立 PTY，支持输入、ANSI 显示、Ctrl-C、窗口 resize 和 Bash 作业控制。PTY 的 stdout/stderr 天然合并，保存到 stdout.log，不能再声称两个流可以分开。普通 `run` 继续使用原来的独立管道。

根进程退出与采集完成仍分开处理：根退出禁用终端输入，存活后代继续采集；worker 结束才标记会话采集完成。关闭页面属于用户结束临时 playground，会清理整个容器。

## Web 与证据

可选 `--browser` 使用 [Chrome 镜像](../docker/playground-chrome/README.md)，
左侧增加 Terminal / Browser 标签页，右侧观察视图和原始事件详情保留。
被跟踪的启动脚本等待图形环境就绪后启动 Chrome，再 exec 为交互 Bash；
Chrome 的网络服务及其他后代复用同一采集链路。Xvfb/Openbox/x11vnc 作为
会话内显示基础设施运行，不放入目标进程树。独立 desktop 模块负责就绪检查、
`/api/sessions/{session}/desktop` WebSocket 的 token/Origin/存活会话校验、
有界转发和取消；仅通过 Docker exec 连接容器 loopback VNC，无额外公开端口。
Chrome wrapper 从既有 OverlayFS 读取任务公开 CA，导入临时容器 NSS 数据库；
宿主信任不变。用户关闭页面或服务关闭时，显示连接和容器统一清理。

默认地址为 `127.0.0.1:8080`，支持 `--listen`。启动时生成随机访问令牌并打印完整 URL；页面和 WS 均校验令牌，浏览器 WS 同时校验 Origin。静态资源在二进制内，不依赖 CDN 或生产环境 Node.js。

`file.open` 增加独立文件预览面板，原始事件 JSON 详情继续保留。预览从事件关联的已保存内容加载，提供文本/JSON/Hex 和复制路径/当前内容；失败、未采集、空文件与截断有独立状态。`GET /api/sessions/{session}/file-preview` 使用同一个访问令牌，仅接受固定快照文件的 offset/length，限制读取大小并拒绝路径符号链接；不允许任意宿主文件路径。旧记录无内容时不发起路径读取。完整边界见[内容检查设计](content-inspection-design.md)。

终端输出使用二进制 WS 帧，状态和 trace 使用 JSON；输入和 resize 转换为现有 worker IPC。右侧按下述进程树规则组织真实事件，原始日志继续落盘。

终端与 trace 面板使用统一深色主题。trace 视图位于底部时跟随新命令和事件滚动；用户向上查看历史时保持阅读位置，回到底部后恢复跟随。滚动仅作用于 trace 面板，不移动整个页面或展开已折叠分支。

### 进程树与节点行为

右侧按进程展示可折叠树：节点只展示命令、PID 和运行/退出状态，不展示线程节点或 TID。同一进程的各线程行为按观察顺序合并；线程创建和单独退出不显示成命令生命周期。所有已知线程退出后才显示该进程结束；由线程创建的真正子进程直接挂到所属进程下。点击行为可在原始事件中查看 PID 与进程生命周期标识。所有命令节点默认收起；运行中新增可见事件短暂闪烁，隐藏子进程的活动也提示在运行中的祖先上。已展开节点继续追加并保留底部跟随；手动向上滚动后保持阅读位置，不重置折叠状态。尊重 reduced-motion。

事件时间默认使用 `observed_unix_ns`（记录器写入时间），新 TCP/UDP 转发汇总优先使用 `data.started_unix_ns`（代理收到连接/首包时间），按浏览器本地时区显示日期和时间，精确到毫秒。显示层按该开始时间排序，时间相同保留接收顺序；原始事件顺序、历史回收和进程状态计算仍保持原来的接收时序。悬停可查看 UTC 开始、完成和写入时间。旧记录缺少开始字段时不推算。进程标题中的 `Elapsed` 列使用 `monotonic_ns` 计算从创建（没有创建事件时从首次观察到的 exec）到最后已知线程退出的经过时间，连续 exec 不重置起点。运行中显示 `Running`，缺少起止时间时显示 `—`。起止时间独立于事件保留窗口，晚到的代理事件不延长已结束进程的耗时；该值包含等待和采集开销，不是 CPU 时间或单次 syscall 的耗时。

核心事件与前端模型统一使用 PID 归属。同一进程寿命内的 process_instance_id 保持不变，非主线程 exec 也延续该身份；PID 再次使用时分配新实例。新 schema v2 不导出线程 ID 或线程任务 ID，创建事件使用 parent_pid、child_pid 和 child_process_instance_id。tracing/processes.rs 在底层把 ptrace 句柄映射到 TGID，内部维护线程存活状态，只有最后一个已知成员退出才导出 process.exit。前端 process-tree.mjs 直接消费进程记录；历史 v1 日志的线程聚合隔离在 legacy-process-tree.mjs 中。

### 事件过滤

右侧顶部的 Event filters 使用原生复选框多选，默认全选。分组开关支持 `file.*`、`memory.*`、`network.*`、`process.*`，组内可单独选择 `file.open`、`memory.mmap` 等具体类型；部分选择显示半选状态。提供 Select all / Clear all。新出现的事件类型加入对应分组并沿用其选择；整组切换同时重置组内单项设置。选择在当前页面持续生效。

过滤仅影响行为行，进程节点、父子关系、名称、状态与耗时仍从完整采集记录计算。顶部显示当前可见事件数，节点及底部显示可见/保留事件数；过滤后没有行为的节点显示 No matching events。原始 JSONL 保留完整记录，恢复勾选可重新显示当前保留窗口内的事件。事件过滤模型位于 event-filter.mjs，与进程身份模型、DOM 渲染分开。

界面保留最近 1,000 条行为，保留仍存活的节点及展示历史所必需的祖先；无剩余行为或后代的已结束节点可回收。祖先行为过期会明确显示省略条数。退出只改变节点状态，不立即删除历史。行为缺少 PID 时显示为未归属，不能猜测所属进程。

文件事件展示路径与返回 FD/错误；`memory.mmap` 展示匿名映射或文件路径、长度、偏移、prot/flags、映射地址或错误及调用耗时；网络事件仅展示代理实际观察到的上游结果、报文和转发字节数；不再混入 ptrace 网络调用。JSONL/WS 统一新增 category（network/file/process/memory/other），kind 保留具体行为，为后续过滤提供稳定字段。各事件携带运行内 `process_instance_id`，fork 携带 `child_process_instance_id`，晚到代理事件沿该身份回到原进程，不复活已退出进程或归给复用 PID 的后续进程。历史已回收时保留状态未知的独立记录，不猜新的父子关系。该标识不恢复 Agent 工具语义。

展示复用 JSONL/WS 协议及原生 `details/summary`，前端事件模型独立于 DOM。平铺表格无法直观看出创建关系，额外服务端树协议会重复已有事件信息，因此保持树投影在前端。

TCP 代理生成 `network.protocol`，展示 HTTP Host/请求目标或 TLS SNI 的域名及来源，并按 `connection_id` 补充当前保留窗口内同一连接的 connect/upstream/transfer 行，保留 IP:port 和原始事件不变。解析限制见[文件与 TCP 采集](file-network-collection.md)。UDP connect/AF_UNSPEC 不产生新的出口事件；仅真实报文产生代理观察。不能按端口 443 推断 HTTPS/QUIC。旧日志的 connect 行仍兼容显示。

终端容器使用 content-box，确保 FitAddon 读取的高度、宽度不包含容器内边距，最后一行和光标不会被页脚裁切；窗口 resize 继续通过原有 PTY 尺寸通道传递。

## 实时流量

`--capture-http` 同时启用 SSE / WebSocket 消息观察，复用现有 TLS 和 PID
关联。Traffic 中点击 SSE/WS 请求的 Messages 页即可查看方向、开始时间、
类型和内容，支持复制及原始详情。到底部时跟随新消息，向上滚动或选旧消息
暂停跟随。窗口推进后，已过期的内容会移除并提示。

消息和索引写入单独的 SQLite 滑动窗口，默认每条 64 KiB、整个运行 4 MiB
且最多 1,000 条。页面仅对当前可见流每 250 ms 查询一次，不把消息正文
塞进全局 trace JSONL。解析、窗口存储、查询和展示各自独立；详见
[内容检查设计](content-inspection-design.md#sse-and-websocket-messages)。

```mermaid
flowchart LR
    Proxy[现有 HTTP / TLS 转发] --> Observer[SSE / WebSocket 旁路解析]
    Proxy --> Destination[原目标 / 应用]
    Observer --> Queue[有界消息队列]
    Queue --> Window[SQLite 滑动窗口]
    Window --> API[鉴权查询 / 过期 ID 返回 410]
    API --> Messages[Traffic Messages 实时视图]
```

## 验证

保留现有运行生命周期测试，补充子命令、PTY isatty/resize/Ctrl-C、访问控制和会话清理测试。用真实 Docker 和浏览器验证命令执行、右侧 trace、窗口缩放、断线清理、服务退出清理。退出回归覆盖向服务进程组连续发送 SIGINT、daemon 已删除但 CLI 返回失败、容器实际残留和状态复核失败；只在测试的 Docker CLI 边界注入故障，实际创建/删除容器。测试不能以静态示例事件代替实际采集。
