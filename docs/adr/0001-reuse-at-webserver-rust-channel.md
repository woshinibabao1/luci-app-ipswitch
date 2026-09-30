# ADR 0001：复用 MT5700 Console 的 AT 通道，自己不开串口

- 状态：**已采纳**
- 日期：定案于项目初版（v1.0.0）

## 背景

本服务要换出口 IP，就必须向 5G 模组发 AT 命令。设备上模组接在
`/dev/ttyUSB1`（115200），而这个串口的持有者是 **MT5700 Console 的 Rust 后端
`at-webserver-rust`** —— 它同时服务着 LuCI 页面（拨号、网络状态、短信、频段…）
一大堆功能。

最直接的实现是：本服务自己 `open("/dev/ttyUSB1")`，写 AT、读响应、完事。
这样不需要依赖任何其他软件包。

## 选项

**A. 自己打开串口，直接发 AT**
- 优点：零依赖，单包自包含，行为完全可控
- 缺点：**串口是独占资源**。两个进程同时读写同一个 tty，表现为字符交叉、响应错位、
  命令与回包对不上号。MT5700 Console 的 LuCI 页面会立刻开始报错，短信功能也可能
  收不到短信。而且一旦本服务崩在「已发命令、未读回复」的中间态，串口可能残留半个
  响应，污染后端进程的下一轮读取
- 另一条隐性成本：AT 通道本身有很多真机坑（见 MT5700 Console 项目红线：
  `CCHO` 不可回收、`CRSM` 只认 `3F00`、通道忙要快速失败…），这些坑已经在那边的
  `atclient.rs` 里踩过一遍了

**B. 做 MT5700 Console 后端的 RPC 客户端**
- 优点：串口仍然只有一个持有者；复用后端已解决的全部真机问题；本服务退化成
  一个纯编排层，代码少、职责单一
- 缺点：**强依赖 MT5700 Console** —— 没装它，本服务就是个不能工作的空壳

**C. 自己起一个串口代理，两边都连它**
- 优点：解耦
- 缺点：等于要重写一遍 `at-webserver-rust` 的核心能力，工作量与风险都比 B 大得多，
  而且设备上多一个常驻进程

## 决策

选 **B**。本服务通过 `127.0.0.1:8765` 的 newline-JSON RPC 下发 AT，
协议为：

```json
→ {"id":1,"method":"at","params":{"cmd":"AT^NDISSTATQRY?","auth_key":"…"}}
← {"id":1,"result":{"success":true,"data":"^NDISSTATQRY: 1,…","error":null}}
```

并在 `root/etc/init.d/ipswitch` 的 `start_service()` 里加一条**依赖提示**：

```sh
[ -x /usr/bin/at-webserver-rust ] || \
	logger -t ipswitch "警告: 未发现 /usr/bin/at-webserver-rust…"
```

即：**缺依赖时仍然启动**，但在启动那一刻就把话说明白（写进 syslog），
而不是等第一次切换报错才发现。

## 后果

- ✅ 串口安全：LuCI 页面、短信等功能与本服务互不干扰，这是最高优先级
- ✅ 代码量小：本服务只做「编排 + HTTP」，不碰任何 AT 协议细节
- ⚠️ 强依赖：安装时必须先装 MT5700 Console。刻意不用 `DEPENDS` 声明这个依赖 ——
  若 feed 里没有 `luci-app-mt5700`，声明 `DEPENDS` 会导致**整个包构不出来**，
  比「装上了但换 IP 失败（且有日志）」更糟。改为**运行时检查 + 日志警告**
- ⚠️ RPC 的承载能力有上限（后端 `MAX_RPC_LINE=8192`、单条 AT 最坏约 13s），
  但本服务下发/接收的都是短命令与短回包，不构成问题
- ⚠️ 后端在串口重连时会主动断开所有 RPC 连接 → 本服务**不做连接池**，
  每次调用新建连接（`rpc.rs` 的 `AtClient::send`）。这是刻意选的行为，不是疏漏

## 相关

- `src/rust/src/rpc.rs`：RPC 客户端
- `root/etc/init.d/ipswitch`：启动时的依赖提示
- MT5700 Console 项目：`src/rust/src/rpcserver.rs`（服务端实现）
