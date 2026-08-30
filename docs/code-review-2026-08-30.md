# TermBridge 全量 Review 交叉验证与修复规划

- 日期：2026-08-30
- 验证方式：7 路子 agent 并行交叉验证 + 关键争议点逐条人工复核源码 + russh 0.62.5 官方 API 签名比对（docs.rs）
- 结论：原 review 整体成立。**全部 21+ 条声明中未发现假阳性**，仅 4 处表述需微调（见 §3），1 处数字待确认。
- 用途：修复规划基线。行号均为验证时实际行号，实施时以语义定位为准。

---

## 1. 逐条验证结论

### P0 — 发布即坏

| # | 声明 | 结论 | 关键证据 |
|---|------|------|----------|
| P0-1 | npm 平台包二进制以 0644 发布，Linux/macOS npx 必然 EACCES | **成立** | [build-platform-packages.mjs:81-92](../packaging/npm-platform/scripts/build-platform-packages.mjs#L81-L92) 生成的 package.json 只有 name/version/description/license/os/cpu/files/publishConfig，**无 bin 字段**；[launcher.js:35](../packaging/npm-platform/launcher.js#L35) `spawnSync(bin, ...)` 直接执行。npm pack 对非 bin 文件的 mode 归一化为 0644 是 npm/pacote 已知行为（原生二进制分发必须走 bin 字段，esbuild 同款方案即如此），与 review 实测结论一致。Windows 不受影响（无执行位概念）。 |

### P1 — 安全

| # | 声明 | 结论 | 关键证据 |
|---|------|------|----------|
| P1-2 | authorized_keys 硬安全规则可被「新文件路径」绕过 | **成立** | 绕过链路完整实锤：(1) [sessions.rs:576-579](../src/application/sessions.rs#L576-L579) Upload 统一映射 `RemoteOperation::Create`；(2) [path_policy.rs:266-283](../src/application/path_policy.rs#L266-L283) 目标不存在时仅对**父目录** canonicalize 并传入 `check_scope_and_safety`；(3) [path_policy.rs:443](../src/application/path_policy.rs#L443) `hard_safety_deny` 用 `canonical.ends_with("/.ssh/authorized_keys")` 匹配的是父目录 `/home/u/.ssh` → 不命中。向不存在的 `~/.ssh/authorized_keys` 上传畅通无阻。authorized_keys2 确实不在匹配列表（见 §3-1 表述修正）。 |
| P1-3 | 控制面 token 确定性可猜解，无速率限制 | **成立** | [server.rs:392-400](../src/transport/control/server.rs#L392-L400) `generate_token()` 注释自称「生成随机 token」但实际 `ts_ns ^ (pid << 64)`，无任何随机源；高 64 位即 pid、低 64 位即纳秒时间戳。[server.rs:107-128](../src/transport/control/server.rs#L107-L128) Windows 用 TCP loopback（注释自认「第一版简化方案」）；[instance.rs:68](../src/transport/control/instance.rs#L68) `std::fs::write` 写 discovery 文件（含 token）无权限设置，XDG_RUNTIME_DIR 缺省时回退 `/tmp`（[instance.rs:147-161](../src/transport/control/instance.rs#L147-L161)）→ 默认 0644。HELLO 握手失败后无限速。 |
| P1-4 | open_session 的 host 参数可注入 ssh -G 选项 | **成立** | [mcp/server.rs:451-469](../src/transport/mcp/server.rs#L451-L469) `params.host` 直接透传（schema 描述亦无约束）；[sshconfig.rs:28-34](../src/infrastructure/sshconfig.rs#L28-L34) `.arg("-G").arg(alias)`，alias 以 `-` 开头（如 `-F/恶意config`、`-oProxyCommand=...`）会被 ssh 当作选项。可绕过 hosts.toml 主机白名单（ADR-0017）；参数虽为数组传递无 shell 注入，但 `Match exec` 在 `ssh -G` 求值时会被执行。 |

### P1 — 功能缺陷

| # | 声明 | 结论 | 关键证据 |
|---|------|------|----------|
| P1-5 | PTY 行列参数全颠倒 | **成立（两处）** | russh 0.62.5 官方签名（docs.rs 已比对）：`request_pty(want_reply, term, col_width, row_height, pix_w, pix_h, modes)`、`window_change(col_width, row_height, pix_w, pix_h)` —— **列在前**。[ssh.rs:393-404](../src/infrastructure/ssh.rs#L393-L404) 传 `rows, cols`（行在前）；[ssh.rs:1222-1227](../src/infrastructure/ssh.rs#L1222-L1227) `window_change(size.rows, size.cols, 0, 0)` 同样颠倒。`PtySize{rows, cols}` 字段语义本身正常（[provider.rs:77-80](../src/domain/provider.rs#L77-L80)），是调用点传参顺序错。80×24 终端远端建成 24×80，每次 resize 再次颠倒。 |
| P1-6 | agentd 会话生命周期三连坏 | **成立（三个子项全实锤）** | (a) `Event::pty_exit`/`Event::session_lost` 定义于 [protocol.rs:119-136](../agentd/src/protocol.rs#L119-L136)（含单测），但 [rpc.rs](../agentd/src/rpc.rs) 全文**无一处构造**；[event_pump](../agentd/src/rpc.rs#L381-L417) 每 10ms 轮询（rpc.rs:388），状态非 Attached 即 `break`（rpc.rs:390-392）——不读增量、不发终止事件 → 尾部 ≤10ms 输出丢失、attached 客户端永远挂等。(b) [rpc.rs:144-188](../agentd/src/rpc.rs#L144-L188) 客户端断连（EOF）后 `break` 无任何 session 清理；[session.rs:174-183](../agentd/src/session.rs#L174-L183) attach 对 `Attached` 状态直接报 `InvalidState`（→ INVALID_STATE），session 永远停在 Attached，重连必须先手动 detach。(c) [session.rs:210-218](../agentd/src/session.rs#L210-L218) `session.pty.lock()`（同步 std Mutex）内做 [pty.rs:117-124](../agentd/src/pty.rs#L117-L124) 的阻塞 `libc::write`，且返回的短写字节数 `n` 被 `?` 丢弃；master fd 为阻塞模式，对不读 stdin 的进程粘贴大块输入 → 持锁无限期挂死 → close/resize 全卡。 |
| P1-7 | agentd fork+exec 无 CLOEXEC，子进程继承所有 fd | **成立** | [pty.rs:46-56](../agentd/src/pty.rs#L46-L56) `openpty` 未设 O_CLOEXEC；fork 后子进程仅显式关 `master_fd` 与 `slave_fd`（pty.rs:63-67），无 closefrom/fcntl 清理 → execvp 后 shell 继承 Unix listener、所有客户端连接、其他 session 的 pty master。威胁表述见 §3-2 修正。 |
| P1-8 | detach_session 先 remove 后校验，普通会话被误杀 | **成立** | [sessions.rs:942-963](../src/application/sessions.rs#L942-L963)：L945-949 先 `sessions.remove()`，L952-957 才 `downcast_ref::<PersistentTerminalHandle>()`；非 persistent 会话返回 InvalidArgument 时已从 map 移除，session 变量 drop → read_task abort、连接释放，注释 L961 自己写明该后果。之后所有调用 SESSION_NOT_FOUND。校验应前置。 |
| P1-9 | sftp_chmod 传 mode:"0" 实际执行 chmod 0000 | **成立** | [server.rs:82-90](../src/transport/mcp/server.rs#L82-L90) `parse_octal_mode` 对 `None`/`""`/`"0"` 均返回 `Ok(0)`；[sftp.rs:273-285](../src/infrastructure/sftp.rs#L273-L285) chmod 无条件 `permissions: Some(mode)`，而对比 [sftp.rs:226-244](../src/infrastructure/sftp.rs#L226-L244) mkdir 明确有 `if mode != 0` 跳过（0=服务器默认 umask）。Agent 传 mode:"0" → 远端 chmod 0000，服务当场锁死。「约定」归属表述修正见 §3-3。 |
| P1-10 | wait_for 的 context_lines≥1 丢失匹配文本本身 | **成立** | [output.rs:586-610](../src/domain/output.rs#L586-L610) `extract_context`：`context_lines==0` 分支返回匹配所在行（正确）；`≥1` 分支只拼 `before = data[..m.start]` 尾部 N 行 + `after = data[m.end..]` 头部 N 行，**`data[m.start..m.end]` 匹配文本本身从未输出**；且 pattern 位于行中时 `after` 首段是匹配行的同行剩余部分。[mcp/server.rs:495](../src/transport/mcp/server.rs#L495) 确认 `context_lines` 直接透传。 |
| P1-11 | ANSI strip 两处系统性泄漏 | **成立** | (a) [ansi_strip.rs:88-93](../src/domain/ansi_strip.rs#L88-L93) 默认分支 `i += 2`，对 `ESC ( B`（charset 选择）这类带 intermediate byte（0x20-0x2F）的序列，尾字节 `B` 泄漏为正文；CSI/OSC/DCS 分支本身正确，缺的是 `ESC + (0x20-0x2F)* + final(0x30-0x7E)` 规则。(b) `strip_control_sequences` 为无状态纯函数（ansi_strip.rs:41），[mcp/server.rs:507-511](../src/transport/mcp/server.rs#L507-L511) 每次调用只处理当页输出 → since_cursor 分页在页边界截断 CSI/OSC：前半被「不完整序列」分支丢弃（ansi_strip.rs:63-66）、后半变正文。需跨调用状态机。 |
| P1-12 | SSH 层锁跨 await + 全链路无超时 → 挂死 | **成立** | (a) [ssh.rs:1117-1126](../src/infrastructure/ssh.rs#L1117-L1126) `open_sftp_provider` 持 tokio Mutex 跨 `SftpProvider::open(session).await`（含 channel_open_session + subsystem 握手）无超时；(b) [ssh.rs:1138-1147](../src/infrastructure/ssh.rs#L1138-L1147) `exec` 持锁跨 `channel_open_session().await` 无超时；(c) [ssh.rs:1240](../src/infrastructure/ssh.rs#L1240) `close()` 与 [ssh.rs:1296](../src/infrastructure/ssh.rs#L1296) `keepalive_loop` 均需先拿同一把锁 → 上述挂死时全部阻塞，session 杀不死；(d) keepalive 自身有 timeout 包裹（1298-1302），但 russh client config 未设 `inactivity_timeout`（ssh.rs 全文 grep 无）；PTY write（[1208-1213](../src/infrastructure/ssh.rs#L1208-L1213)）与 `ssh -G`（[sshconfig.rs:28-34](../src/infrastructure/sshconfig.rs#L28-L34) `.output().await`）均无超时。半开连接 → 上述 future 永不返回。 |

### P2 — 值得排期修（子 agent 定位 + 内容与 review 一致）

| # | 声明 | 结论 | 位置 |
|---|------|------|------|
| P2-1 | 策略层敏感路径用原始路径匹配，可被 `//etc`、`/tmp/../etc/`、`~/../etc/` 绕过 Confirm | 成立 | [policy.rs:278-307](../src/application/policy.rs#L278-L307)（PathPolicy 后置 realpath 归一化，此层没有） |
| P2-2 | hosts.toml 解析失败 fail-open，typo 即全局放行 | 成立 | [host_policy.rs:174-213](../src/application/host_policy.rs#L174-L213) 仅 WARN，未区分「不存在」与「损坏」 |
| P2-3 | SFTP upload 非原子（create=TRUNCATE 直写目标）；download_dir 远端文件名直拼 Windows 路径 | 成立 | [sftp.rs:97-135](../src/infrastructure/sftp.rs#L97-L135)（对比 download 有 tmp+fsync+rename）、[sftp.rs:507](../src/infrastructure/sftp.rs#L507) |
| P2-4 | agentd 无升级路径：version 写而不比，二进制非原子覆盖部署 | 成立 | [persistent.rs:906-934](../src/infrastructure/persistent.rs#L906-L934)、784-787 |
| P2-5 | agentd 日志写 stdout，proxy/bootstrap 模式下打爆协议帧流 | 成立 | [agentd/main.rs:63-65](../agentd/src/main.rs#L63-L65) `tracing_subscriber::fmt()` 默认 stdout（CLI 侧特意用了 stderr） |
| P2-6 | fork 后子进程 malloc，与自身 SAFETY 注释矛盾 | 成立 | [pty.rs:71-84](../agentd/src/pty.rs#L71-L84) `CString::new(dir)`、`Vec<CString>` 均在 fork 与 execvp 之间分配 |
| P2-7 | 僵尸进程不 reap；kill 不打进程组 | 成立 | [session.rs:341-344](../agentd/src/session.rs#L341-L344)、[pty.rs:143-145](../agentd/src/pty.rs#L143-L145) |
| P2-8 | CLI read_msg 无长度上限（4GiB 分配），agentd 侧 twin 有检查 | 成立 | [cli/protocol.rs:114-121](../cli/src/protocol.rs#L114-L121) vs agentd/src/protocol.rs |
| P2-9 | Secret derive Debug，Zeroizing 派生 Debug 打印明文 | 成立 | [credential.rs:45-48](../src/domain/credential.rs#L45-L48) |
| P2-10 | GUI 双读循环，StrictMode 挂载两次抢读同一 PTY | 成立 | [gui/src-tauri/src/main.rs:80](../gui/src-tauri/src/main.rs#L80) + [TerminalView.tsx:80](../gui/src/components/TerminalView.tsx#L80) |
| P2-11 | bootstrap 公钥命令单引号注入；非 UTF-8 home `to_str().unwrap()` panic | 成立 | [bootstrap.rs:264-267](../src/application/bootstrap.rs#L264-L267)、226 |

### P3 — 优化建议（择要）

| # | 声明 | 结论 | 位置 |
|---|------|------|------|
| P3-1 | `notify_one` 并发 waiter 丢唤醒 | 成立 | [output.rs:131](../src/domain/output.rs#L131)，wait_for 走 select 双路等待确有多 waiter 场景 |
| P3-2 | close_session 文档说幂等但二次调用报 SESSION_NOT_FOUND | 成立 | [mcp/server.rs:573-582](../src/transport/mcp/server.rs#L573-L582) vs [sessions.rs:504-516](../src/application/sessions.rs#L504-L516) |
| P3-3 | timeline 满容量 `Vec::remove(0)` O(n) | 成立 | [timeline.rs:125-131](../src/domain/timeline.rs#L125-L131)，数据结构为 Vec |
| P3-4 | redact 漏 URL userinfo（`postgres://root:pw@db`） | 成立 | [redact.rs:20-55](../src/infrastructure/redact.rs#L20-L55) 仅覆盖凭证 key=value / Authorization / PEM |
| P3-5 | Unix 上 agentd 本地缓存路径依赖 cwd（只读 LOCALAPPDATA） | 成立 | [persistent.rs:1073-1082](../src/infrastructure/persistent.rs#L1073-L1082) |
| P3-6 | ssh -G 的 known_hosts 路径含空格被 split_whitespace 拆分 | 成立 | [sshconfig.rs:105-112](../src/infrastructure/sshconfig.rs#L105-L112)（解析侧 bug，非注入） |
| P3-7 | release.yml：`!cancelled()` 语义反了；tag↔Cargo.toml 无一致性校验；「兼容旧版嵌套归档」分支永不触发 | 成立 | [release.yml:172](../.github/workflows/release.yml#L172)、194-195、208-212。嵌套归档时 `ls "$d"/termbridge*` 匹配到顶层目录名本身 → 条件永假。OIDC 配置本身（id-token、Node 24、--access public、先平台包后主包）验证正确。 |
| P3-8 | 未提交改动遗漏：launcher 失败提示仍指旧包名；packaging/npm/README.md 过时；两份主 README 未提 npm 渠道 | 成立 | [launcher.js:42](../packaging/npm-platform/launcher.js#L42) `npx -y termbridge-mcp`（npm 上不存在）；[packaging/npm/README.md](../packaging/npm/README.md) 与 release.yml 实际逻辑矛盾 |

---

## 2. Review 中「复核过没问题」的部分

以下沿用原 review 结论，本轮未重复验证：环形缓冲游标/回绕/截断数学（output.rs、agentd/buffer.rs）、agentd framing 边界检查、rmcp 工具层参数钳制、PID/EOF→Lost 的 ADR 语义依据、OIDC 发布配置、launcher 平台映射、MCP stdio 透传。

## 3. 表述修正（原 review 4 处需微调，结论本身不变）

1. **P1-2 的 authorized_keys2**：「sshd 默认也读」→ 实际 **OpenSSH 8.7（2021-08）已完全移除 authorized_keys2 支持**，仅 ≤8.6 的旧服务器仍读。修补仍有价值（兼容旧 sshd），但优先级可放低，不必与 authorized_keys 同级。
2. **P1-7 的威胁建模**：「跨会话可读写别的 session 的 master（注入按键/偷输出）」偏强——shell 自身不会主动读写继承的 fd，需进程内恶意代码配合探测。**主要确定性危害是 fd 泄漏导致 EOF 语义破坏**：其他 session 的 master 被无关进程持有 → 永不 EOF → session Lost 检测失效、proxy 模式 EOF 永远等不到（review 后半句自洽）。修复价值不变。
3. **P1-9 的「解析约定」归属**：`「0/空 = 服务器默认」`是 `parse_octal_mode` 的通用行为（服务于 mkdir 的 0=umask 语义，测试名 `parse_octal_mode_none_or_empty_returns_zero` 可证），**chmod 工具的 schema 描述并未承诺 0=默认**（只说 "Mode is octal string like '755'"）。准确表述：chmod 复用了 mkdir 的解析函数却没复用其 mode==0 跳过语义，Agent 模仿其他工具「传 0 表示默认」的习惯即触发 chmod 0000。修复方向不变：chmod 拒绝 mode 0 或要求显式 "0000"。
4. **P1-11(b) 的「默认 64KB」**：分页+无状态 strip 必然跨界泄漏这一点确凿（方向正确），但 `since_cursor` 的 max_bytes 默认值 64KB 本轮未逐字验证，实施时确认即可，不影响修复方案。

另有一处 review 未展开、验证中确认的事实：agentd event_pump 为 **10ms 轮询**（rpc.rs:379 注释自认 MVP、Phase 4 待事件化）——这既是 P1-6(a)「≤10ms 输出丢失」的来源，也意味着修 P1-6(a) 时应与「轮询改 Notify 事件化」一并设计，避免修成「轮询 + 补发事件」的过渡形态。

---

## 4. 修复规划

### 批次 0 —— 发版阻塞（打 v0.3.0 tag 前必须完成）

| 项 | 修复要点 | 验证方式 |
|----|----------|----------|
| P0-1 | build-platform-packages.mjs 生成的平台包 package.json 增加 `bin` 字段，三个二进制全部列入（npm pack 对 bin 文件保留 0755）；launcher.js:42 失败提示同步改为 `@summerxzp/termbridge-mcp` | 本地 `npm pack` 后 `tar tvf` 检查 mode 位；Linux 容器内 npx 冒烟 |

### 批次 1 —— 核心体验（P1-5/6/7）

| 项 | 修复要点 | 验证方式 |
|----|----------|----------|
| P1-5 | ssh.rs:397-398 与 :1224 两处交换 rows/cols 实参 | 远端 `stty size` 断言 24 80；resize 后再断言 |
| P1-6(a) | pty_read_loop 检测 EOF/PID 退出后：flush 尾部增量、构造 `pty_exit`/`session_lost` 事件；event_pump 退出前保证已发终止事件；顺带设计轮询→Notify 事件化 | e2e：kill 远端 shell，断言客户端收到 session_lost 且 buffer 尾部无丢失 |
| P1-6(b) | rpc.rs 请求循环退出（EOF/写失败）时对本次连接 attach 过的 session 执行 detach（状态 Attached→Detached） | 断开重连场景：直接 attach 成功，不再需要手动 detach |
| P1-6(c) | send_input 改为：循环写直到写完（处理短写）；写 PTY 改非阻塞 + poll 或放入独立阻塞线程（`spawn_blocking`），不持 std Mutex 跨阻塞系统调用 | 粘贴 >64KB 到 `cat > /dev/null` 类不读 stdin 进程，close/resize 不被卡死 |
| P1-7 | fork 前对需要保留的 fd 设 FD_CLOEXEC（listener、客户端 conn、其他 master），或 fork 后子进程 `close_range` 清理再 dup2 | /proc/<pid>/fd 检查 shell 进程的 fd 列表 |

### 批次 2 —— 安全（P1-2/3/4）

| 项 | 修复要点 | 验证方式 |
|----|----------|----------|
| P1-2 | Create 分支：父目录校验通过后，**对展开后的目标路径本身**再做一次 hard_safety_deny（suffix 匹配不依赖 realpath）；补 `authorized_keys2` 匹配（低优先，见 §3-1） | 单测：上传到不存在的 `~/.ssh/authorized_keys` 被拒 |
| P1-3 | token 改 `rand::random`（或 OS 随机源）；instance discovery 文件写后 chmod 600；HELLO 失败加限速/断连；Windows 侧规划 Named Pipe（TODO 已有） | 枚举 /tmp/termbridge 文件权限断言 600 |
| P1-4 | sshconfig::resolve 入口拒绝 `-` 开头的 alias（其他下沉符号一并考虑 `--`）；MCP schema 补 pattern 约束 | 单测：host="-F/x" 报错 |

### 批次 3 —— 其余 P1（P1-8/9/10/11/12）

| 项 | 修复要点 |
|----|----------|
| P1-8 | detach_session：先 downcast 校验 persistent，成功后再 remove |
| P1-9 | sftp_chmod 对 mode==0 返回 INVALID_ARGUMENT（要求显式 "0000"），或 chmod 实现复用 mkdir 的跳过语义（推荐前者，语义更明确） |
| P1-10 | extract_context 的 ≥1 分支补上 `data[m.start..m.end]`，after 侧从下一行起取 |
| P1-11 | (a) 默认分支按 `ESC + (0x20-0x2F)* + (0x30-0x7E)` 规则消费；(b) strip 状态机跨调用化（Session 级持有状态，或 since_cursor 分页读改为「按完整序列边界对齐」） |
| P1-12 | open_sftp_provider/exec 的 channel_open 包 `tokio::time::timeout`；connect/auth/ssh -G/PTY write 同理；可选：russh config 设 inactivity_timeout 兜底 |

### 批次 4 —— P2 按模块顺手修

- **agentd 模块**：P2-5（日志改 stderr）、P2-6（fork 前预构造 CString/argv，消除 fork 后分配）、P2-7（EOF 后 waitpid reap；kill 改 `kill(-pid)` 打进程组）。
- **SFTP 模块**：P2-3（upload 改 tmp+rename 原子写；download_dir 做文件名 sanitize，处理 `\`、`:`、保留名）。
- **持久化模块**：P2-4（agentd.version 比较触发重新部署；部署改 tmp+chmod+rename）。
- **策略模块**：P2-1（Confirm 匹配前先 canonical 化）、P2-2（hosts.toml 区分不存在/损坏，损坏时 fail-closed 或明确报错）。
- **CLI/GUI**：P2-8（read_msg 补长度上限，与 agentd 对齐）、P2-10（GUI 读任务防重入）、P2-11（bootstrap 命令注入转义 + 非 UTF-8 home 容错）、P2-9（Secret 手写 Debug 输出 `[REDACTED]`）。

### 批次 5 —— P3 与文档

- P3-7：release.yml 去掉 `if: !cancelled()`（默认 success）；加 tag↔Cargo.toml 版本一致性校验步骤；删除或修正「兼容旧版嵌套归档」死分支（glob 匹配目录本身）。
- P3-8：packaging/npm/README.md 重写或删除；两份主 README 补 npm 安装章节（与 GitHub Release 渠道并列，注明 npm 为主渠道）。
- 其余 P3（notify_waiters、close 幂等、VecDeque、redact URL userinfo、XDG 路径、known_hosts 空格）随手修。

### 建议执行顺序

批次 0 → 1 → 2 → 3 → 4 → 5。批次 0 是一行 JSON 的事，先解除发版阻塞；批次 1 决定核心卖点（远程持久会话、PTY 正确性）是否成立；批次 2 在公开发布前必须完成。每批完成后跑 `cargo test` + 相关 e2e 脚本（examples/ 下已有 phase6_reconnect / phase7a_t16_resize 等，可扩为回归）。
