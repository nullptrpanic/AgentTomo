# Playground 工具镜像

`agenttomo-playground:24.04` 基于 Ubuntu 24.04，是 `agenttomo-box playground` 的默认镜像。

需要网页内操作 Chrome 并观察其行为时，使用可选的 [Chrome 镜像](../playground-chrome/README.md) 和 `--browser`；加上 `--capture-http` 可注入 Chrome 的 CA 信任并采集 HTTPS 内容。

在仓库根目录构建：

```bash
docker build --pull=false -t agenttomo-playground:24.04 docker/playground
```

需要本地已有 `ubuntu:24.04` 基础镜像，并能访问 Ubuntu apt 软件源。构建只发送这个目录作为上下文，不打包项目源码或日志。工具在构建时安装，Playground 启动会话时直接使用本地镜像，不运行 apt 或自动拉取镜像。

| 用途 | 工具 |
| --- | --- |
| 网络接口与连接 | `ifconfig`、`netstat`、`route`（net-tools），`ip`、`ss`（iproute2），`iptables`、`ip6tables` |
| HTTP、DNS 与连通性 | `curl`、`wget`、`dig`、`nslookup`、`ping`、`nc`、`traceroute` |
| 进程与诊断 | `ps`、`top`、`pgrep`、`pstree`、`lsof`、`strace`、`tcpdump` |
| 文件、文本与编辑 | `jq`、`rg`、`tree`、`file`、`less`、`nano`、`vi`、`zip`、`unzip`、`xz` |
| 开发与远程客户端 | `git`、`python3`、`ssh`、`scp`、CA 证书 |

直接进入镜像：

```bash
docker run --rm -it agenttomo-playground:24.04
```

通过 Playground 使用：

```bash
cargo run -p agenttomo-box -- playground

# 也可以显式指定其他本地兼容镜像。
cargo run -p agenttomo-box -- playground --image my-compatible-image:latest
```

网络后端要求内核支持 namespace 内的 IPv4/IPv6 TPROXY 和策略路由。运行时仍以当前用户 UID/GID 执行，清空 capabilities，启用 no-new-privileges，为嵌套 user/net namespace 使用 seccomp=unconfined、apparmor=unconfined（开发用，放宽容器策略）。查看接口和普通 HTTP/DNS 请求可以直接使用；修改网络配置、抓包、附加跟踪其他进程等操作仍受容器权限限制。

已有容器不会随镜像重建而变化；使用新版本启动 Playground，再创建会话即可使用新镜像。

验证时使用 UID/GID 1000、cap-drop=ALL 和 no-new-privileges 检查工具存在及基本操作；Playground 的 Docker 集成测试还会在默认镜像的终端中运行 ifconfig、ip、curl、dig、jq 和 Python。真实浏览器验证包含 curl 请求本地 HTTP 服务及 dig 查询 DNS，对应 TCP/UDP 事件均正常记录。
