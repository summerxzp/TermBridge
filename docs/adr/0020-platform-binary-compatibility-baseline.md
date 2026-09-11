# ADR-0020：三平台发布二进制兼容性基线

- **Status**: Accepted
- **Date**: 2026-09-11
- **Phase**: 0.3.4
- **Supersedes**: —
- **Depends on**: [ADR-0009](0009-bootstrap-host-and-credential-provider.md)（auth-helper）、[ADR-0019](0019-unix-credential-prompt-fallback.md)（Unix 凭据输入）
- **Amends**: —（不改运行时行为，只改构建/发布策略）

## 1. Context

### 1.1 起因：v0.3.3 发版后实测发现 Linux 产物无法在 Ubuntu 22.04 上运行

`npx @summerxzp/termbridge-mcp@latest` 在 Ubuntu 22.04（glibc 2.35）上启动失败：

```text
/lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.39' not found
```

根因（实测锁定）：

- CI 用 `ubuntu-latest`（当前 = Ubuntu 24.04，glibc 2.39）构建 Linux 产物
- `rustix 0.38.44`（crossterm 依赖）引用 `pidfd_spawnp` / `pidfd_getpid`
  **弱符号**（运行时探测型），但在 glibc 2.39 机器上**编译**时，链接器把
  版本引用写死为 `GLIBC_2.39`
- 同一份源码在 Ubuntu 22.04 本机构建，GLIBC 要求仅 2.34
- 即：**构建环境问题，不是代码问题**；且 `ubuntu-latest` 是漂移标签
  （22.04 已进退役表，26.04 已 preview），产物 glibc 基线会随 runner 无声上涨

### 1.2 兼容面实测盘点（v0.3.3 产物，2026-09-11）

| 平台 | 产物实测 | 兼容面 |
|---|---|---|
| Linux x86_64（gnu 动态） | `objdump -T`：GLIBC 要求 2.39（0.3.1 起如此，非新引入） | 仅 Ubuntu 24.04+ / Debian 13+ / Fedora 40+。**排除 Ubuntu 20.04/22.04、Debian 11/12、RHEL/Rocky/Alma 8/9、Alpine**。开发者自己的 22.04 机器都跑不了 |
| Windows x86_64（msvc 动态 CRT） | 导入表含 `VCRUNTIME140.dll` | 需目标机装有 VC++ 2015-2022 redistributable。Win10/11 桌面版多数已有，但精简镜像 / Server Core / portable 开发环境可能缺失，症状与 Linux glibc 同构（缺运行时库 → 起不来） |
| macOS arm64 | Mach-O `LC_BUILD_VERSION`：**minos=11.0**，SDK=26.5 | macOS 11+（Big Sur，2020）。**deployment target 由 Rust target 内置默认锚定，不随 runner SDK 漂移**——评审担心的「macOS 也有 glibc 式漂移」实测不成立 |
| agentd（远端 Linux x86_64） | 同 Linux gnu 产物 | **最严重**：agentd 经 SFTP 部署到完全不可控发行版的服务器，glibc 赌注风险最大化 |

### 1.3 musl 可行性验证（2026-09-11 本机完整实测）

- `x86_64-unknown-linux-musl` 静态编译：termbridge / termbridge-mcp /
  termbridge-auth-helper / termbridge-agentd **全部通过**
- 静态验证：`file` = static-pie，`ldd` = statically linked，**0 个 GLIBC 引用**
- 功能验证：MCP initialize 握手成功；auth-helper 弹窗级联路径（坏 DISPLAY
  → unsupported 带指引）+ TTY 密码输入（script PTY 实测）正常
- **agentd 46/46 测试在 musl 下全绿**
- 体积：musl strip 后 12.8MB vs gnu 17.4MB（**更小**）

### 1.4 DNS/NSS 影响面分析（实测收敛）

musl resolver 不支持 glibc 的 NSS 扩展。对 TermBridge 的影响面经代码核查收敛为：

- **agentd：零影响**——源码无任何主机名解析（只 listen 本地 Unix socket）
- **本地 TermBridge：影响点唯一**——`russh client::connect((hostname, port))`
  处的 `ToSocketAddrs`（含 ProxyJump bastion 解析，同为本地侧）
- 受影响场景：ssh config `HostName` 使用 mDNS（`.local`）/ SSSD / LDAP
  企业主机名 / 非标准 NSS backend 的用户
- 不受影响：IP 直连、标准 DNS 域名、`/etc/hosts`——覆盖绝大多数使用方式

## 2. Decision

### 2.1 平台矩阵（本 ADR 核心决策）

```text
本地（Local Host）
├── Windows x86_64     MSVC + 静态 CRT（crt-static）     Win10+（见 §2.4）
├── Linux x86_64       musl 静态                          任意现代 x86_64 Linux 内核
└── macOS arm64        aarch64-apple-darwin，锚定 deployment target 11.0

远端（Remote Runtime）
└── agentd Linux x86_64  musl 静态（与本地 Linux 同产物线）
```

**Linux 只发布 musl 静态产物，不保留 gnu 产物**（无双产物，见 §2.6）。

### 2.2 Linux：musl 静态（唯一产物）

- CI 构建改 `--target x86_64-unknown-linux-musl`（runner 装 musl-tools），
  产物名不变（`termbridge-linux-x86_64.tar.gz` / npm `linux-x64` 包名不变）
- agentd 同 musl——远端部署从「赌目标机 glibc」变为「上传自包含 ELF +
  chmod + 运行」，与项目 Remote zero-install 原则一致
- CI 增加**静态性验证**：`file`（期望 static-pie）+ `objdump -T | grep -c
  GLIBC`（期望 0）——防止未来依赖引入动态引用而无人察觉
- CI 增加 musl 下的 agentd 测试（46/46）与 auth-helper 集成测试

**措辞纪律**：对外文档写「statically linked, no glibc / runtime library
dependency」，**不写 100% 兼容**——kernel 仍是边界（静态二进制仍走
syscall），musl 解决的是 libc 兼容性，不是任意内核兼容性。

### 2.3 Windows：静态 CRT（crt-static）

**现状**：v0.3.3 产物动态链接 `VCRUNTIME140.dll`。

**缺失率评估**（TermBridge 用户画像 = 装 MCP 客户端的开发者）：

- VSCode / Cursor / Trae **安装器版**会装系统级 VC++ redist → 已覆盖
- 风险人群：**portable 版 VSCode 用户**（redist 只在应用目录，不进
  system32）、精简系统镜像、Windows Server Core、纯 Claude Code（Node）
  且无其它 MSVC 软件的环境
- 评估结论：缺失概率**低但非零**，且失败模式对用户完全不透明（MCP
  server 静默起不来，用户看到的是「TermBridge 不工作」而非「缺 DLL」）——
  排障成本远高于预防成本

**体积代价**（npm 元数据实测：win32-x64 包 unpacked 27.4MB，其中三个
exe 共 25.2MB）：

- 静态 CRT 增量典型 0.3–1.5MB/exe → 包整体 **+3%（乐观）～ +16%（悲观）**
- zip 压缩后增量约为未压缩的 30–50%（CRT 代码高度可压缩）
- 对比收益：彻底消除一类「装了也跑不起来」的工单

**决策**：CI 构建加 `RUSTFLAGS="-C target-feature=+crt-static"`。Rust
msvc target 官方支持该特性（等价 /MT）；本项目仅链 ring 的 C 汇编、无
C++ STL 依赖，无已知坑；ring 自身 CI 即测试 crt-static。auth-helper 的
CredUI 调用（comctl32/credui）是系统 DLL，不受 CRT 静态化影响。

### 2.4 Windows 最低版本

`x86_64-pc-windows-msvc` 产物基线：Windows 10+ / Server 2016+（与最新
VC++ v14 redist 支持面一致；crt-static 后不再依赖目标机 redist 版本）。
ConPTY（Windows Terminal 时代 API）在 Win10 1809+ 可用，与基线兼容。
**不在产物中显式声明更低版本支持**。

### 2.5 macOS：维持 arm64-only，显式锚定 deployment target

- **不做 Universal Binary**：Intel Mac 用户走 Rosetta 2（README 已声明）；
  Universal 会使所有可执行物双架构化（体积近乎翻倍），而 Intel Mac 占
  比持续萎缩，收益不抵复杂度。有真实需求再评估（lipo 标准路线可后补）
- **显式设置 `MACOSX_DEPLOYMENT_TARGET=11.0`**：当前 Rust target 默认即
  11.0（产物 Mach-O 实测 minos=11.0），但显式写入 CI 是防未来漂移的
  保险——runner 升级 / 依赖变化不会无声抬高基线
- 凭据输入维持 ADR-0019 的 osascript 方案（系统自带），原生 AppKit
  dialog 仍按 ADR-0019 §2.8 缓议
- macOS 无本地 agentd（agentd 仅 Linux），发布复杂度可控

### 2.6 明确不做的事

- **双 Linux 产物（gnu + musl）**：一旦双产物出现，「用户下载哪个 /
  npx 分流 / agentd 推哪个 / README 怎么写 / 未来 ARM64 再 ×2」的矩阵
  复杂度立即爆炸。项目当前最宝贵的是核心路径简单可预测，一个静态
  Linux x64 把问题彻底关掉
- **Linux ARM64**：无用户诉求前不增加（远端 ARM 服务器有真实需求时，
  agentd 单独加 `aarch64-unknown-linux-musl` 即可，本地侧不受影响）
- **macOS Universal / Intel**：见 §2.5
- **容器内构建 / zig 交叉构建**：musl 已覆盖其全部收益，无需引入额外
  工具链
- **静态 glibc**：glibc 官方不支持静态链接（NSS 动态加载），不考虑

### 2.7 文档措辞（README 兼容性声明）

按「兼容性边界」而非「缺陷清单」的口径：

> Linux binaries are statically linked with musl and require no glibc or
> other runtime libraries. Standard DNS and `/etc/hosts` hostname
> resolution are supported; environments relying on NSS-specific
> integrations (mDNS `.local`, SSSD/LDAP hostnames) should resolve the
> host via standard DNS name or IP.

Windows 侧对应声明：statically linked CRT, no Visual C++
Redistributable installation required.

## 3. 实现落点（0.3.4）

| 文件 | 变更 |
|---|---|
| `.github/workflows/release.yml` | Linux job：musl-tools + `--target x86_64-unknown-linux-musl` + strip；Windows job：`RUSTFLAGS=-C target-feature=+crt-static`；macOS job：显式 `MACOSX_DEPLOYMENT_TARGET=11.0`；新增静态性验证 step（file/objdump 断言） |
| `.github/workflows/ci.yml` | 增加 musl target 的 check + agentd 测试 + helper 集成测试；Windows check 加 crt-static |
| `README.md` / `README.en.md` | 平台兼容性表更新（musl 静态 / 无 VC redist 依赖 / macOS 11+）+ §2.7 措辞 |
| `CHANGELOG.md` | 0.3.4 段落 |

产物名、npm 包名、目录布局**全部不变**——用户无感知升级。

## 4. 验证清单（0.3.4 发版预检追加）

- [ ] Linux 产物：`file` = static-pie；`objdump -T` GLIBC 引用 = 0
- [ ] Windows 产物：导入表无 `VCRUNTIME140.dll` / `ucrtbase.dll`
- [ ] macOS 产物：Mach-O minos = 11.0
- [ ] musl 产物在 Ubuntu 22.04（glibc 2.35）真实运行（本机即可验）
- [ ] agentd musl 46/46（CI Linux job 内）
- [ ] auth-helper 弹窗级联 + TTY 路径（musl，CI 或本机）
- [ ] npx 实测三平台包安装 + MCP initialize 握手

## 5. Consequences

- Linux 兼容面从「Ubuntu 24.04+ 等新发行版」扩展到「任意现代 x86_64
  Linux（含 RHEL 8 / Debian 11 / Alpine / Ubuntu 20.04）」；开发者自己的
  22.04 机器恢复可用
- 远端 agentd 部署不再依赖目标机 libc——「Remote zero-install」承诺
  从口号变为结构保证
- Windows 消除 VC++ redist 缺失导致的静默启动失败（代价：包体积
  +3%~16%）
- macOS 基线显式化为 11.0，防 runner/依赖漂移
- 已知限制（接受并文档化）：musl 无 NSS 扩展——mDNS/SSSD 主机名场景
  用 IP 或标准 DNS 绕过；musl malloc 多线程分配性能弱于 glibc（本项目
  I/O 型负载，影响可忽略）
- `ubuntu-latest` 仍可用于 CI（构建环境），但**不再决定运行时 ABI**——
  musl 静态产物与 runner 的 glibc 版本彻底解耦
