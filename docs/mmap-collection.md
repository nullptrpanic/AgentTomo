# mmap 调用采集

## 范围与实现

在现有 Linux x86_64 原生 ABI 的 seccomp/ptrace 链路中观察 `mmap`，供普通 `run` 和 Playground 共用。过滤器对 `SYS_mmap` 返回 `SECCOMP_RET_TRACE`；`tracing` 将入口交给 `collectors/memory`，并复用现有待完成调用状态获取退出结果。完成记录以 `source=ptrace`、`kind=memory.mmap` 写入现有 JSONL/WS 通道。

`mmap` 同时支持文件和匿名内存，参数也不同于文件打开调用，因此由独立的内存采集器解释。沿用现有 `Call`、`result_fields`、PID/process_instance_id 和记录组件，不给文件打开解析器增加条件分支，不建立映射区间索引或新协议层。

## 记录字段

| 字段 | 含义 |
| --- | --- |
| `pid` / `process_instance_id` | 复用采集器的进程及运行内进程寿命身份，同一进程线程共享；未知 PID 为 null |
| `syscall` | `mmap` |
| `address` | 请求映射地址，十六进制字符串 |
| `length` | 请求长度，字节 |
| `prot` / `flags` | 原始权限和标志位 |
| `fd` / `offset` | 文件描述符和字节偏移；匿名映射中 FD 被内核忽略 |
| `anonymous` | 是否设置 `MAP_ANONYMOUS` |
| `resolved_path` / `path_bytes` | 文件映射入口处读取 `/proc/<tid>/fd/<fd>` 得到的路径快照及原始字节 |
| `path_capture` / `capture_error` | 文件路径读取成功为 `complete`，失败为 `unreadable` 并记录错误；匿名映射不读取路径 |
| `result` / `errno` / `restart` | 原始 syscall 返回值、错误号及内核重启标记，与现有事件约定一致 |
| `mapped_address` | 成功时返回的映射地址（十六进制字符串），失败或重启时为 null |
| `started_unix_ns` / `completed_unix_ns` | 采集器观察到 syscall 入口和返回时的绝对时间 |
| `start_source` | `syscall_entry`，与记录器接收及写入时间分别保存 |
| `duration_ns` | 采集入口处理到观察到 syscall 返回的单调时间差，包含跟踪开销 |

地址使用字符串避免浏览器 JSON 数值精度损失。文件路径是入口观察到的 FD 路径，可能与并发关闭/复用/重命名竞态；不能作为不可变文件身份。读取失败不改变目标调用，路径不是系统调用直接传入的参数。原始参数和返回值始终保留。

## 展示与边界

Playground 在实际调用任务下显示 `memory.mmap`，摘要包括文件路径或匿名映射、长度、偏移、权限/flags、结果和调用耗时；点击事件继续查看原始字段。现有绝对时间、进程归属、树结构和滚动行为复用。

本次只观察进入内核的 `mmap` 调用，不改写其参数或结果，不读取映射内容。libc 在用户态拒绝的调用（例如未对齐偏移）没有 syscall 停点，不产生事件。映射后的普通内存读写、缺页、回写、`munmap`、`mprotect`、`mremap`、`msync`、32 位 `mmap2` 不在范围内。`MAP_SHARED` 可能写回文件，`MAP_PRIVATE` 的私有修改不能当作文件写入；不会生成逐次内存访问或文件修改事件。失败事件不创建成功映射；没有 syscall 返回停点时可能没有完成记录。动态加载器的 mmap 也会被观察，增加事件数量及跟踪开销。

## 验证

真实 C 程序覆盖匿名映射、忽略的 FD、共享/私有文件映射、关闭 FD 后继续访问、非零偏移、非 UTF-8 文件名、零长度/坏 FD/非对齐偏移失败，以及线程/子进程归属。Rust 检查目标返回行为、实际文件内容和事件证据；前端模型检查摘要不把映射地址当成 FD，并覆盖失败/重启。真实 Docker 浏览器验证事件到界面的链路。

2026-11-05 验证记录：

- `cargo test -p agenttomo-box --test mmap --offline`：3 项通过；添加采集实现前，这些用例均因缺少 mmap 事件而失败。未对齐偏移通过原生 `SYS_mmap` 测试内核错误，本机 libc 包装函数会提前拒绝，已用 strace 确认。
- `node --test crates/agenttomo-box/tests/process-tree.test.mjs`：21 项通过，包含新增 mmap 摘要测试。
- `cargo test --workspace --offline -- --nocapture`：62 项报告通过，6 项需显式启用的 Docker 用例未运行；其中 TCP/UDP 的两个 IPv6 条件用例因宿主没有 `::1` 而提前返回，不计作 IPv6 功能验证。
- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --offline -- -D warnings`、`cargo build -p agenttomo-box --offline` 和 `git diff --check` 通过。
- 使用默认 `agenttomo-playground:24.04` 镜像，在独立临时服务的真实浏览器终端执行 Python 文件与匿名映射。143 条命令执行阶段事件中，文件映射显示 PID 12、4,096 字节、偏移 4,096、返回地址与 0.085 ms 调用耗时；关闭 shell 后会话完整收到 147 条事件，其中 66 条为 mmap（包含加载器映射）。原始事件选择、绝对时间、桌面/窄屏显示通过，未发生页面 JS 错误，服务停止后验证容器已删除。

机制参考：[mmap(2)](https://man7.org/linux/man-pages/man2/mmap.2.html)、[seccomp(2)](https://man7.org/linux/man-pages/man2/seccomp.2.html)。
