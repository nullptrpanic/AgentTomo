# secguard IPv4 TCP/UDP 验证脚本

`secguard.sh` 是一个单文件 Linux PoC，用于在新的 user/network namespace 内运行命令，并透明转发 IPv4 TCP、UDP 出站流量及对应回包。普通 DNS 查询和 TCP 回退使用相同转发路径，不实现 DNS 解析器，也不修改 `/etc/resolv.conf`。

## 使用

以普通用户运行，无需 sudo。依赖现有的 `bash`、`python3`、`capsh`、`unshare`、`ip`、`iptables`，没有第三方 Python 依赖。

```bash
bash ./secguard.sh -c '/bin/bash'

bash ./secguard.sh --mode redirect -c \
  "curl -q -4 --noproxy '*' --connect-timeout 10 --max-time 30 -v https://www.qq.com/"
```

- `direct`：透明转发，不输出流量日志。
- `redirect`：同样透明转发，额外输出 TCP 连接、UDP 出站数据报的原始五元组 JSON。日志仍与子命令共用 stdout，与旧版一致。
- `--noproxy '*'` 仅用于测试时排除容器里已有的代理变量；脚本不要求应用配置代理。
- 目标命令退出时，脚本返回其退出码并结束这次 namespace 中的进程。

## 转发方式和权限

TCP 继续使用 `OUTPUT REDIRECT` 和 `SO_ORIGINAL_DST`。UDP 使用 `OUTPUT` 标记、策略路由和 `PREROUTING TPROXY`，通过 `IP_RECVORIGDSTADDR` 取得每个数据报的原目标。

内层管理进程创建 TCP 接收 socket、UDP 接收 socket 和 UDP 回包 raw socket，通过已有 UNIX socketpair 把三个 FD 交给外层管理进程。FD 始终属于创建它的内层网络 namespace。外层发往真实目标的 TCP/UDP socket 是普通 socket。

UDP 回包保留远端源 IP、源端口和数据报边界，计算 UDP 校验和，由 Linux 填写 IP 校验和。内层 raw socket 只用于回包，不需要在外层创建 raw socket。使用这个发送方式可避免回包 socket 绑定的端口与应用自身端口冲突。回包带有独立 mark，防止再次被拦截。

执行目标命令之前，清空其 effective、permitted、inheritable、bounding、ambient capabilities，保留 `no_new_privs=1`。管理 socket 和控制 FD 不传给目标命令。

## 环境要求

除之前验证过的 user/network/mount/PID namespace 和 TCP REDIRECT，新增 UDP 路径还要求：

- 内层可配置 `ip rule` 和策略路由。
- 内核及 iptables 支持 UDP `TPROXY` 和 `MARK`。
- 内层允许 `IP_TRANSPARENT`、`IP_RECVORIGDSTADDR`。
- 创建内层 raw socket 的管理进程具有该 namespace 内的 `CAP_NET_RAW`，且外层 seccomp/LSM 没有禁止对应系统调用。

启动时先在新 namespace 内用一个测试数据报验证 UDP 捕获、原目标识别、校验和及原源地址回送，测试包不离开该 namespace。能力不可用或自检失败时，脚本在执行目标命令之前失败，不会静默退回仅支持 TCP 的模式。它不加载宿主机模块、不改变外层容器配置、不使用 TUN/FUSE/eBPF，也不挂载新的 `/proc`。

## 范围和限制

- 本次范围为 IPv4 单播 TCP/UDP 出站及对应回包。IPv6、广播/组播、主动入站和 ICMP 错误转发不在本次实现中。
- 所有目标地址，包括 `127.0.0.0/8`，都按外层容器的目标解释，保留原脚本的行为。因此可以连接外层的 `127.0.0.11` DNS stub；内层应用自己启动的 localhost 服务不会自动得到普通本地直连语义。
- DNS 配置中的服务器需要通过外层 IPv4 网络可达。经 IPv4 DNS 查询得到 AAAA 记录并不表示具有 IPv6 联网能力。
- UDP 按应用源 IP/端口复用一个外层 socket，不同目标共享该端点的外层源端口；回包保留实际来源，因此允许服务器切换回复端口。活跃端点可以收到其他来源的数据报，应用使用 connected UDP 时仍由内层内核按其对端过滤；脚本不额外实现来源白名单。
- UDP 空闲会话 120 秒后回收，最多 1024 个；达到上限或发送缓冲区满时丢弃数据报并报告错误。保留零长度数据报；最大 IPv4 UDP payload 为 65507 字节，真实外层路径仍受 MTU 等条件限制。
- 外部服务看到的是外层转发 socket 的地址/端口；不承诺保留原始 IP 选项、TTL、源端口、所有 socket 错误和时序。
- 这是网络转发 PoC，不是完整文件系统沙箱，也未验证对恶意工作负载的完整隔离。继承的 `/proc` 仍呈现外层 PID 视图。

## 验证

本地可运行的标准库测试：

```bash
bash -n secguard.sh
python3 tests/test_secguard.py -v
```

在目标 Linux 容器中，以普通用户运行完整集成测试：

```bash
python3 tests/test_secguard.py --integration -v
```

集成测试在外层创建临时 TCP/UDP 服务，再通过真实 `secguard.sh` 访问，覆盖：

- TCP 大数据和半关闭。
- UDP 多目标、连续回包、connected/unconnected socket、零长度及 65507 字节数据报。
- 原源地址/端口回送，以及应用与远端使用相同端口的情况。
- 外层 loopback 和非 loopback IPv4 目标；如果外层没有非 loopback IPv4 地址，单独跳过后者。
- DNS UDP 报文和 TCP 回退，不访问公网 DNS，也不改 resolver 配置。
- 子命令五类 capability 清零、no_new_privs、FD 不继承、stdin、退出码及后台进程清理。

当前开发机是 macOS，未运行 Linux namespace/TPROXY/raw socket 集成测试。可移植测试通过不能代替目标容器的集成验证。

参考：[Linux TPROXY](https://docs.kernel.org/networking/tproxy.html)、[Linux raw sockets](https://man7.org/linux/man-pages/man7/raw.7.html)、[UDP 协议](https://www.rfc-editor.org/rfc/rfc768.txt)。
