# luci-app-ipswitch

给 **Hiveton H5000M**（MT7987A + 鼎桥 MT5700M-CN 5G 模组）用的一个常驻小服务：
收到一个 HTTP 请求，就**换一次出口 IP**，换完在响应流里吐一个标记。

它被写出来是为了对接 [zgyd（移动套餐获取）](../../) 的 `-ip-switch` 开关 ——
zgyd 需要一个「**GET 一下就换 IP、看到标记文本就算成功**」的 HTTP 端点，
而路由器上本来没有任何东西提供这个端点。

- 换 IP 的两种手段：**重新拨号**（默认） 或 **切换 APN**（在预设池里轮换）
- 语言：**Rust**（单一静态二进制，musl，`opt-level="s"` + LTO，release 约几百 KB）
- 完成判定：**以模组侧状态为准**（见下文「完成判定」）
- 它**自己绝不打开模组串口** —— 所有 AT 命令都借道 MT5700 Console 后端下发
- **状态**：已在真机（Hiveton H5000M）上确认**能切换出口 IP**（2026-09-30 实测）。
  交叉编译产物已由 CI 出包（见第 9 节）

## 0. 定位（先看这一节，别搞混）

这件事有两层，**两层的答案正好相反**，混起来会走错路：

| 问题 | 答案 | 含义 |
|---|---|---|
| 这个插件是不是独立的？ | **是** | 独立仓库、独立包名（`ipswitch`）、独立 init 脚本。**不并入 MT5700 Console** 的仓库，也不改它的任何代码 |
| 运行时要不要 MT5700 Console？ | **要** | AT 命令借道它的后端（`127.0.0.1:8765`）。**没装它就换不了 IP** |

一句话：**包是独立的，AT 通道是依赖的。**

换句话说，本插件把自己定位成 MT5700 Console 后端的一个客户端，
而不是"再实现一份 AT 能力"。原因见下一节 —— 串口只有一个，
多一个持有者不会"多一个功能"，只会把两边一起弄坏。

**这不是一个可以单独装完就走的插件**：安装顺序是先 MT5700 Console，再本插件。

---

## 1. 为什么不自己开串口

设备上 `/dev/ttyUSB1` 同一时刻**只能有一个持有者**，而这个持有者是
`at-webserver-rust`（MT5700 Console 的后端进程，PID 因重启而变，端口固定）
—— 它同时服务着 LuCI 页面、短信、网络状态等一堆功能。

再起一个进程去抢串口，结果不是「多一个功能」，而是**把 LuCI 页面和短信一起弄坏**。
所以本插件把自己定位成 MT5700 Console 后端的一个 **RPC 客户端**：

```
zgyd ──GET /switch──▶ ipswitchd ──newline-JSON/TCP──▶ at-webserver-rust ──▶ /dev/ttyUSB1 ──▶ 模组
 :8790                    ▲                                :8765（只监听 127.0.0.1）
                          │
                    （不碰串口）
```

这也意味着：**必须先装 MT5700 Console**，否则本服务能启动但换 IP 一定失败
（init 脚本会在启动时打一条 `logger` 警告提醒你）。

> 曾经考虑过"自带串口实现、顺便检测后端在不在"的版本 —— 被否掉了。
> 理由：那会让同一个功能有两套 AT 实现，出问题时无法确定是哪一套在说话；
> 而"串口归属"这件事本来就该只有一个答案。取舍的完整论证见 `docs/adr/0001`。

---

## 2. 安装

### 方式一：用预编译包（推荐）

从本仓库的 GitHub Actions 产物里取对应你固件包管理器的那个文件。
最近一次成功构建出的产物（每个约 362 KiB）：

| 固件 | 包格式 | CI artifact 名 | 实际文件名 |
|---|---|---|---|
| ImmortalWrt / OpenWrt **SNAPSHOT**（apk） | `.apk` | `ipswitch-apk-aarch64_cortex-a53` | `ipswitch-1.0.0-r1.apk` |
| OpenWrt **23.05 / 24.10**（opkg） | `.ipk` | `ipswitch-ipk-aarch64_cortex-a53` | `ipswitch_1.0.0-1_aarch64_cortex-a53.ipk` |

```sh
#apk
apk add --allow-untrusted ./ipswitch-1.0.0-r1.apk

#opkg
opkg install ./ipswitch_1.0.0-1_aarch64_cortex-a53.ipk
```

装完 `uci-defaults` 会自动 `chmod +x`、`enable` 并 `start`。

**升级不会丢配置**：`/etc/config/ipswitch` 在包定义里声明为 `conffiles`
（apk 侧是 `protected_paths`），所以升级时你填过的 APN 池、marker、超时都会保留。
反过来说，**新版本新增的配置项不会自动出现在你的旧配置文件里** ——
这没关系，程序对缺项一律用默认值兜底（`Config::from_uci`），
需要时 `uci set` 补上即可。

### 方式二：从源码编译

需要 OpenWrt SDK（见 `scripts/sdk-build.sh`，脚本封装了完整流程）：

```sh
# 本机（Linux 或 WSL）
./scripts/sdk-build.sh aarch64_cortex-a53 aarch64-unknown-linux-musl 1.0.0
#                        └ARCH            └Rust triple                     └版本
# 产物：./out/aarch64_cortex-a53/ipswitch_*.apk（或 .ipk）
```

或者放进自己的 feed：

```sh
ln -s /path/to/luci-app-ipswitch package/ipswitch
make menuconfig   # Network ──▶ ipswitch
make package/ipswitch/compile V=s
```

---

## 3. 配置

配置文件：`/etc/config/ipswitch`。**改完执行 `/etc/init.d/ipswitch reload`**
—— 配置只在进程启动时读一次（避免同一次切换的中途配置变了、前后半段按不同参数跑）。

| 选项 | 默认值 | 说明 |
|---|---|---|
| `listen` | `0.0.0.0` | HTTP 监听地址。**安全性说明见第 6 节** |
| `port` | `8790` | HTTP 端口 |
| `marker` | `IP切换完成` | 写进响应流、供调用方判定「切换完成」的标记文本。**必须与调用方 `-ip-switch-marker` 一致** |
| `method` | `redial` | `redial` = 只重拨，不动 APN；`apn` = 在 `apn_list` 轮换后重拨 |
| `apn_list` | *(空)* | APN 池，逗号分隔。**必须填真实可用的 APN**（`cmnet` / `3gnet` / `ctnet` …） |
| `apn_protocol` | `IP` | 写进 `AT^SETAUTODIAL` 第 3 参数：`IP` / `IPV6` / `IPV4V6` |
| `dial_mode` | `1` | `AT^SETAUTODIAL` 第 2 参数（手册 16.18）：`0`=模组内部拨号、`1`=上位机拨号(USB，**H5000M 的默认形态**)、`2`=上位机拨号(网口) |
| `timeout` | `45` | 单次切换最长等待秒数。**必须小于调用方的等待上限** |
| `wan_iface` | `MT5700M` | WAN 接口名（设备上是 `eth2`），用来判断「是否已经拿到地址」 |
| `rpc_host` | `127.0.0.1` | MT5700 Console 后端地址。**后端只监听回环，别改成外网地址** |
| `rpc_port` | `8765` | 后端 RPC 端口 |

### 一个容易踩的坑：超时要配对

```
zgyd -ip-switch-timeout 60   ── 必须 > ──▶   ipswitch timeout 45
```

zgyd 默认等 60s，本服务默认 45s（留 15s 余量给网络往返）。
**两边都调大时要同步调**，否则可能：本服务还在等模组拨号，zgyd 已经超时放弃并进入重试。

### 另一个坑：APN 填错 = 直接断网

`method='apn'` 时如果池里是错的 APN（或空 APN），模组会被网络拒绝并**进入退避窗口**
（`zgyd`/`dial.js` 里记录过这个真机现象）。所以：

- `apn_list` 留空时，`method='apn'` 会**自动退化为 `redial`**（不会去发空 APN）
- 但池里填了错值，本服务**没法替你判断对错** —— 请填运营商真实可用的 APN

---

## 4. HTTP 接口

> 本节是概览。**完整接口契约（含状态码、响应头、字段表、上游 RPC 协议、错误响应）
> 见 [`docs/api.md`](docs/api.md)。**

### `GET /switch` —— 换一次 IP

响应 `HTTP/1.1 200`，`Content-Type: text/plain; charset=utf-8`，
**`Transfer-Encoding: chunked`**（进度边跑边发），`Connection: close`。

成功时的响应体形如（一行一帧，边跑边发）：

```
开始切换出口 IP（策略 redial）
切换前 MT5700M 地址: 117.136.12.34
等待连接断开…
  等待连接断开完成（拨号状态=Some(false)）
重新拨号（APN 保持原配置）
等待拨号完成…
  等待拨号完成完成（拨号状态=Some(true)）
  接口续约方式: ubus
切换结束：117.136.12.34 → 117.136.45.67，用时 22.7s
IP切换完成
切换成功：117.136.12.34 → 117.136.45.67（redial，用时 22.7s）
```

（若中间状态有变化，会插入 `  拨号状态=Some(true)` 这类行；
`wait_dial` 只在状态**变化时**打一行，不会 700ms 一条把日志刷爆。）

失败时**只输出 `切换失败：<原因>`，绝不包含标记文本** ——
调用方据此判为未完成，自行决定重试还是上报。

**特性（都有测试钉死）：**

- 切换跑在独立任务里，**调用方中途断开连接，切换仍会执行完**
- 多个请求**串行执行、不交错**（内部有互斥锁），不会出现两条 AT 命令命令交叉下发
- 每次请求独立连接（不做连接池 —— 后端在串口重连时会主动断开所有 RPC 连接）

### `GET /status` —— 只读状态

只读，**不下发任何写命令**。返回 JSON：

```json
{
  "wan_iface": "MT5700M",
  "wan_ip": "10.76.139.149",
  "dial_active": true,
  "dial_raw": "^NDISSTATQRY: 1,...",
  "method": "redial",
  "marker": "IP切换完成",
  "apn_pool_size": 0,
  "timeout_secs": 45,
  "last_switch": {
    "method": "redial",
    "apn": null,
    "before": "117.136.12.34",
    "after": "117.136.45.67",
    "saw_link_down": true,
    "renewed_by": "ubus",
    "elapsed_secs": 22.7
  }
}
```

注意 `dial_active` 是 `null` 表示**本次没读出来**（与 `false`「读出来了、是未连接」严格区分）。

### `GET /` 或 `GET /health` —— 存活探测

返回 `200` + `ipswitchd ok`。不碰模组，可当心跳用。

---

## 5. 完成判定：为什么是「设备侧状态」

判断「IP 换好了没有」有几种可能的口径，本服务选的是**最保守的那个**：

| 口径 | 问题 |
|---|---|
| 固定等 N 秒 | 网络慢就假成功，网络快就白等 |
| 轮询外网 IP | 需要外网依赖；且要接一个「IP 查询服务」，多一个失败点 |
| **模组已连接 + 接口拿到地址** ← 本服务 | 只依赖本地设备状态 |

实现上是**双条件**（`switcher.rs` 的 `confirm_ready`）：

1. 模组 `AT^NDISSTATQRY?` 回 `^NDISSTATQRY: 1,...`（= 已连接），**且**
2. WAN 接口（`MT5700M`）真的有一个 IPv4 地址

**为什么不能只看接口地址**：DHCP 租约可能长达 6 天，PDP 断了 netifd 并不一定会重跑 DHCP
—— 接口上会**残留上一次的地址**，看起来"有地址"其实是死的。
（`root/etc/hotplug.d/net/99-mt5700-renew` 这个钩子存在的理由，就是这个真机坑。）

于是切换完成后还要主动 `renew` 一次接口（优先 `ubus call network.interface.MT5700M renew`，
失败回退 `ifdown/ifup`），并记录生效方式到 `renewed_by`。

**附加的诚实标记**：如果整轮下来**没观察到链路中断**（`saw_link_down: false`），
成功信息里会附一句「注意：未观察到链路中断」—— 这通常意味着刚才那次「断开」实际没生效，
IP 大概率没变。这是留给人的提示，不改变「成功/失败」的判定。

---

## 6. 安全说明

### 监听地址

默认 `listen='0.0.0.0'`，即**局域网内任何设备**都能触发换 IP。
安全性依赖 **OpenWrt 默认的 WAN 侧 input 策略**（WAN 入站默认拒绝）。

- 如果你把防火墙改成了放行 WAN 入站，**请自己加规则限制来源**，或把 `listen` 改成
  `192.168.10.1` 之类的内网地址
- 改 `listen` 后记得 `reload`

进程启动时若检测到监听在 `0.0.0.0`，会在 syslog 打一条提示。

### AT 参数注入

APN / 用户名 / 密码最终会被拼进 AT 命令的**引号里**，一个 `"` 就能提前闭合引号、
拼出第二条 AT 命令。前端（LuCI）有 `sanitizeAtParam`，但那**挡不住手写配置文件**，
所以后端**必须再过滤一遍**：`config.rs` 的 `sanitize_at_param` 只放行
`[A-Za-z0-9._-]`，并截断到 99 字节。

### 不碰串口

重申：本服务**不打开** `/dev/ttyUSB1`，不发任何周期上报类 AT 命令
（比如 `^PDCPDATAINFO`，那种命令一旦下发会永久常驻）。所有操作都是短平快的
「设置 → 查询 → 确认」，跟 LuCI 页面走的是同一条通道。

---

## 7. 与 zgyd 对接

```sh
zgyd -ip-switch http://192.168.10.1:8790/switch
```

zgyd 侧的契约（`pkg/ipswitch.go`）：

- 发 `GET`，要求 `HTTP 200`
- **边收边找**标记文本（保留 `len(marker)-1` 字节的尾窗，防止标记被分块切在中间）
- **看到标记即成功，不等 EOF** —— 所以本服务把标记放在收尾块里，但进度会先流出去

默认标记两边都是 `IP切换完成`。若你要改，**两边都要改**：

```sh
uci set ipswitch.config.marker='换好了'
uci commit ipswitch && /etc/init.d/ipswitch reload

zgyd ... -ip-switch-marker '换好了'
```

其余相关开关（默认值取自 zgyd `pkg/config.go`）：

| 参数 | 默认 | 与本服务的关系 |
|---|---|---|
| `-ip-switch-every` | `50` | 每 50 个请求换一次 IP —— 决定了本服务被调用的频率 |
| `-ip-switch-timeout` | `60`（秒） | **必须 > `ipswitch.timeout`**（默认 45） |
| `-ip-switch-retries` | `2` | 失败重试次数。重试会**再打一次** `/switch`，本服务内部串行化，不会交叉下发 AT |

---

## 8. 故障排查

| 现象 | 先查 |
|---|---|
| `切换失败：连接 AT 后端失败` | `ps \| grep at-webserver-rust`、`logread -e at-webserver`；确认 `rpc_port` 与后端一致 |
| `切换失败：切换超时` | 看 `logread -e ipswitch` 停在哪一步；弱信号下拨号可能真超过 45s，可调大 `timeout`（记得同步 zgyd）|
| 返回了标记但 IP 没变 | `/status` 看 `saw_link_down` —— 若是 `false`，说明「断开」没生效 |
| `APN 在校验/回读后发生变化` | 模组侧拒绝了写入；`method='apn'` 时检查池里的 APN 是否真实可用 |
| 服务反复重启 | `logread -e ipswitch`；`respawn 3600 5 5` 触发即说明配置有致命错误（如端口非法）|

日志：本服务把 stdout/stderr 都交给 syslog（`logread -e ipswitch`）。

---

## 9. 开发

```sh
cd src/rust
cargo test              # 48 个用例：39 单元 + 9 端到端
cargo check --all-targets
cargo fmt
```

端到端用例（`src/rust/src/e2e_tests.rs`）会**起一个真的 TCP 假后端 + 真的 HTTP 假客户端**，
把 `HTTP → httpd → switcher → rpc → 假模组` 整条链路跑通，覆盖：

- 成功路径（完整 AT 序列）与失败路径（**失败绝不吐标记**）
- 拨号一直不回时的超时判定
- 调用方中途断开、切换仍跑完
- 并发请求串行化不交错
- `/status` 只读、未知路由 404、`/health` 存活
- `method=apn` 的 APN 轮换

CI（`.github/workflows/build.yml`）两段，**已实跑验证通过**：

1. `check`：`rustfmt --check` + `cargo test` + `cargo check --all-targets` + shell 语法/执行位检查
   —— 实测 50 秒（热缓存）
2. `package`：`needs: check`，用 `openwrt/sdk` 容器矩阵交叉编译 aarch64-musl，出 `.apk` 与 `.ipk`
   —— 两条并行，实测 ipk 2 分 51 秒、apk 4 分 26 秒

产物（实测，第 36705305094 次运行）：

| CI artifact 名 | 大小 |
|---|---|
| `ipswitch-apk-aarch64_cortex-a53` | 370,349 字节 |
| `ipswitch-ipk-aarch64_cortex-a53` | 371,674 字节 |

另有两个 `build-log-*` 附件，编译失败时先下它。

交叉编译这块踩过三个"静默失败"，全部记在 `docs/adr/0005`：
顶层 Makefile 里禁止 `$(error)`、SDK 选中包之后必须回读断言、
apk 与 ipk 的产物名分隔符不同（连字符 vs 下划线）。
**改构建脚本前请先读那一条 ADR** —— 这三个坑的共同点是
`make` 全返回 0 或给出与真因无关的报错。

---

## 10. 已知限制与取舍

- **依赖 MT5700 Console**：没装它，本服务启动得了但换不了 IP（刻意如此 —— 宁可启动后失败并留日志，也不做一个会抢串口的"自包含"版本）
- **端口固定 8790**：与 MT5700 Console 的 8765 错开，避免混淆
- **`dial_mode` 默认 1**：针对 H5000M 的 USB-CDC-NCM 形态；换设备要核对
- **不做外网 IP 校验**：见第 5 节，这是刻意的取舍，代价是「DHCP 给了同一个地址」这种情况会被判为成功（`saw_link_down` 提示可作辅助判据）
- **Rust 二进制随包交叉编译**：本机没有工具链，**任何 Rust 改动都必须在 CI 或 SDK 里验证**（CI 已跑通，直接 push 即可）
- **`conffiles` 只保护 `/etc/config/ipswitch`**：`/etc/init.d/ipswitch` 与二进制属于包本体，升级时会被正常替换

架构决策的详细论证见 `docs/adr/`。
