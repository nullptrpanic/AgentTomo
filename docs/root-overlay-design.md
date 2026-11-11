# 一级目录与 SSL OverlayFS

## 范围与启动方式

`run` 和 `playground` 的 `--overlay-root` 只覆盖能整目录挂载的根目录一级目录，
额外覆盖 `/etc/ssl`。这是部分目录的写入隔离，未覆盖区域保留容器原文件系统。

```bash
./target/debug/agenttomo-box playground --listen 0.0.0.0:18889 \
  --overlay-root --capture-http
```

选择规则：

- 枚举 `/` 的真实一级目录，跳过符号链接、只读目录、豁免目录，以及包含子挂载或豁免路径的目录。
- 符合条件的目录整体建立一个 OverlayFS，不继续向下拆分。
- `/etc/ssl` 单独覆盖；如果已经被一个选中的上级目录覆盖，则不重复挂载。
- 仍沿用 `--overlay-exempt`，已有绝对路径才能豁免。与某个候选目录相交的豁免会使该整个候选目录不被覆盖。
- 被选中的挂载操作失败时停止启动，不在运行失败后静默改为写回原目录。

当前 Playground 默认豁免 `/dev`、`/proc`、`/sys`、`/run`、`/records`。
Chat 控制 socket 位于已豁免的 `/records/control`，保证采集器与沙箱启动器访问同一 socket。
浏览器通信路径 `/tmp/.X11-unix` 和 `/tmp/agenttomo-desktop` 也保留原挂载。
当前 Docker 布局中的 `/etc`、`/usr` 含有子挂载；带浏览器时 `/tmp` 也含有子挂载，
因此这些目录整体跳过，只有 `/etc/ssl` 额外覆盖。未开启浏览器且没有其他子挂载或
豁免路径时，`/tmp` 可以整体覆盖。

## 选择依据

原方案递归拆分锁定挂载边界，并提前复制边界上的零散文件。当前 Chrome 镜像实测约
80 个 OverlayFS，初始化私有 tmpfs 约 8.1 MiB，主要复制成本来自 `/usr/sbin`。
用户选择先缩小覆盖范围，减少挂载数量及复制工作。直接使用单个根 OverlayFS 在当前
继承锁定子挂载的 user namespace 中返回 `EINVAL`；本次不增加 Docker `SYS_ADMIN`。

收窄范围后，同一 Chrome 镜像及挂载配置实测新增 12 个 OverlayFS，开启 CA 注入后的
私有 tmpfs 占用约 184 KiB。测量未启动 Chrome 工作负载，不包含挂载内核元数据、程序
内存和后续写入；目录数量会随镜像及实际子挂载变化。

未覆盖区域的写入会修改容器原文件系统，包括 `/etc/ssl` 之外的 `/etc`、当前 `/usr`、
带浏览器时的 `/tmp` 和根目录的零散文件。它们不受私有 tmpfs 容量上限约束。

## 挂载与存储

文件系统准备仍由容器内 `__tracee` 在现有启动门禁之前完成：

1. 读取 mountinfo、枚举候选目录、校验豁免以及 CA 路径的实际覆盖范围。
2. 创建私有 mount namespace，用 `open_tree` 保存原根的递归挂载视图。
3. 在任务临时目录挂载有容量上限的 tmpfs，将保存的原根视图接入其 `root` 目录。
4. 只在选中的目录上挂载 OverlayFS；upper/work 位于同一个私有 tmpfs。
5. `pivot_root` 到组装好的视图，脱离旧根，注入 CA、收回管理能力，再启动目标。

保存的是挂载视图，不是文件内容副本。此操作在创建存储 tmpfs 之前完成，目标看到的
临时存储锚点为空，不暴露 upper/work；目录符号链接、原有子挂载和只读标志保持不变。
lower 文件内容不提供一致性快照，外层并发修改仍可影响任务读取。

`--overlay-root-size-mib` 默认 512 MiB，是所有 upper/work 共用的 tmpfs 容量上限，
按需占用。启动不再提前复制 `/etc`、`/usr/sbin` 等边界文件。普通文件由内核按需
copy-up；upper 根目录继承原目录的权限、所有者、时间和可读取的扩展属性/ACL。
运行时容量耗尽返回 `ENOSPC`。不持久化 upper/work。

## CA、生命周期与兼容性

开启 HTTP 采集时，证书目录、bundle 和任务 PEM 的真实路径必须位于已选择的 OverlayFS
中。符号链接指向未覆盖的 `/etc` 文件、豁免路径或只读路径时，启动失败，防止注入 CA
污染原系统。未开启 `--overlay-root` 时，沿用仅覆盖 `/etc/ssl/certs` 的原有方案。

挂载跟随目标及后代的 namespace 存活，主命令退出不会提前销毁子进程的文件系统。
收集结束后删除外层空锚点，内核在最后引用释放后回收存储。Chrome 继续使用普通用户
及自身沙箱，采集器使用原根验证上游证书。文件预览仍通过目标 FD 获取内容。

跨 OverlayFS 挂载点的 rename/硬链接可能返回 `EXDEV`，挂载点本身的删除或重命名可能
返回 `EBUSY`。依赖现有 Linux user/mount namespace、OverlayFS、`open_tree`、
`move_mount` 和 `pivot_root` 支持；外层安全策略不允许时明确报错。
Playground 沿用现有 root 引导与完整 UID/GID 映射；普通 `run` 仍取决于所在环境是否
允许读取候选目录并保留其所有者和元数据，任意单 UID 映射环境不保证可用。

## 验证

`tests/root_overlay.rs` 使用真实 Docker 验证选中目录 CoW、未选中区域写回、只读子挂载
保留、CA 不写入原系统、不提前复制大文件、存储隐藏、后代存活和容量上限。
`AGENTTOMO_TEST_ROOT_OVERLAY=1` 运行 `tests/browser-smoke.mjs` 检查 Chrome、HTTPS
解密、文件预览和 UI。浏览器模式下 `/tmp` 保留原视图，私有写入断言使用 `/root`。
