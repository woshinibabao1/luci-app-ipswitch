# ============================================================================
# ipswitch —— OpenWrt 包定义
# ----------------------------------------------------------------------------
# 这是一个**纯后端服务包**（没有 LuCI 页面），因此用标准 package.mk，
# 而不是 luci.mk —— luci.mk 会额外塞进菜单/资源路径，对本包毫无用处。
#
# 它对外提供一个 HTTP 端点：收到 `GET /switch` 就换一次 5G 模组的出口 IP
# （切 APN 或只重新拨号），完成时返回「IP切换完成」标记。
# ============================================================================

include $(TOPDIR)/rules.mk

PKG_NAME:=ipswitch
PKG_VERSION:=1.0.0
PKG_RELEASE:=1

PKG_LICENSE:=MIT
PKG_LICENSE_FILES:=LICENSE
PKG_MAINTAINER:=woshinibabao1 <ajmd007@qq.com>

# 源码随包提供（src/ 下的 Rust 工程在构建期交叉编译），不走下载。
# ★ 刻意**不**手写 PKG_BUILD_DIR：package.mk 的默认值才是权威
#   （`$(BUILD_DIR)/target-$(BOARD)_$(ARCH)/…`）。自己用 $(BUILD_DIR) 拼一个
#   会漏掉 target 那一层，而 package.mk 用的是 `?=`，谁生效取决于定义顺序
#   —— 这种"两个地方各说一套"的坑不值得留。本文件与 src/Makefile
#   一律只引用 $(PKG_BUILD_DIR) 变量本身，值由 package.mk 定。

include $(INCLUDE_DIR)/package.mk

# ---- OpenWrt ARCH → Rust musl target 映射 ----
#
# ★★★ 这一段**绝对不能出现 `$(error)`**，这是个极难定位的坑：
#
#   OpenWrt 生成 `tmp/.config-package.in` 时，会对**每个包**跑一次
#   `make -C package/<name> DUMP=1` 来收集元数据。那次解析一旦失败，
#   包就**不会进 metadata**，后果是：
#     · 包在 menuconfig 里凭空消失
#     · 往 .config 写 CONFIG_PACKAGE_xxx 会被 defconfig 当成**未知符号丢掉**
#       （连 `# ... is not set` 都不留）
#     · `make package/<name>/compile` 变成空目标，清理一下就**返回 0**
#   —— 而**一句错误消息都看不到**（子 make 的 stderr 没有透出来）。
#
#   所以：解析期一律给安全值，真正的报错推迟到 Build/Compile。
#
# 大小端注意：mips*_24kc 是 big-endian（mips），mipsel* 是 little-endian。
# OpenWrt 的 ARCH 是 `<cpu>_<variant>` 形态（如 aarch64_cortex-a53），
# 穷举不可能穷尽 —— 所以下面既有精确表，也有按 CPU 家族的前缀兜底。
RUST_TRIPLE_aarch64_cortex-a53 := aarch64-unknown-linux-musl
RUST_TRIPLE_aarch64_cortex-a55 := aarch64-unknown-linux-musl
RUST_TRIPLE_aarch64_generic    := aarch64-unknown-linux-musl
RUST_TRIPLE_x86_64             := x86_64-unknown-linux-musl
RUST_TRIPLE_mipsel_24kc        := mipsel-unknown-linux-musl
RUST_TRIPLE_mips_24kc          := mips-unknown-linux-musl
RUST_TRIPLE_arm_cortex-a7      := armv7-unknown-linux-musleabihf
RUST_TRIPLE_arm_cortex-a9      := armv7-unknown-linux-musleabihf

# 优先级：外部显式传入 > 精确表 > 前缀推断。
# 外部传入是 scripts/sdk-build.sh 的做法（`export RUST_TRIPLE=...`），最权威。
#
# ★ 这里用 `ifeq ($(strip ...),)` 而不是 `?=`：make 的 `?=` 在变量"已定义但为空"
#   时不赋值，于是环境里一个空的 RUST_TRIPLE 就能把整条兜底链短路掉。
#   判空则不受此影响。
ifeq ($(strip $(RUST_TRIPLE)),)
  RUST_TRIPLE := $(RUST_TRIPLE_$(ARCH))
endif

ifeq ($(strip $(RUST_TRIPLE)),)
  # 前缀兜底。★ 顺序要紧：`mipsel%` 必须排在 `mips%` 之前，
  # 否则 mipsel_24kc 会被 mips% 抢走、编出大端二进制。
  ifneq ($(filter aarch64%,$(ARCH)),)
    RUST_TRIPLE := aarch64-unknown-linux-musl
  else ifneq ($(filter mipsel%,$(ARCH)),)
    RUST_TRIPLE := mipsel-unknown-linux-musl
  else ifneq ($(filter mips%,$(ARCH)),)
    RUST_TRIPLE := mips-unknown-linux-musl
  else ifneq ($(filter arm%,$(ARCH)),)
    RUST_TRIPLE := armv7-unknown-linux-musleabihf
  else ifneq ($(filter x86_64%,$(ARCH)),)
    RUST_TRIPLE := x86_64-unknown-linux-musl
  endif
endif

export RUST_TRIPLE

define Package/ipswitch
  SECTION:=net
  CATEGORY:=Network
  TITLE:=出口 IP 切换服务（切 APN / 重新拨号）
  URL:=https://github.com/woshinibabao1/luci-app-ipswitch
  # 不写 DEPENDS: 本服务的硬前提是「设备上存在 MT5700 Console 后端」，
  # 但那是个 provides 出来的能力名（at-webserver-rust）。把它写成硬依赖，
  # 一旦构建源的 feed 里没有 luci-app-mt5700，会直接让整个包构建失败 ——
  # 代价远大于收益。改为运行时检查：init.d 启动时记一条明确警告，
  # 切换时若 AT 通道不可用也会给出可读的报错。
endef

# ★ 必须声明 conffiles：`/etc/config/ipswitch` 是**用户会改**的文件
#   （APN 池、marker、超时都要按设备实际情况填）。
#   不声明的话，升级包时用户改过的配置会被新包里的默认值直接覆盖 ——
#   而且不会有任何提示。对 ipk 是 opkg 的 conffiles 语义，
#   对 apk 则转成 protected_paths。
define Package/ipswitch/conffiles
/etc/config/ipswitch
endef

define Package/ipswitch/description
  按需切换 5G 模组的出口 IP：可以换 APN，也可以只重新拨号。

  对外提供 HTTP 端点（默认 http://<路由器>:8790/switch），收到 GET 请求即执行
  一次切换，并把进度流式返回，最后输出「IP切换完成」标记。设计上直接对接
  zgyd 的 -ip-switch 参数 —— 抓取过程中每发 N 个请求就换一次出口 IP，
  用来绕开上游对单个 IP 的配额限制。

  ★ 依赖 MT5700 Console（luci-app-mt5700）提供 AT 通道：
    本服务**不直接打开模组串口**（串口同一时刻只能有一个持有者），
    只通过 127.0.0.1:8765 向其后端下发 AT 命令。
    未安装时本服务仍可启动，但切换会明确报「AT 通道不可用」。
endef

# 源码就在本目录下，直接拷；顺带剔掉宿主机的构建产物，
# 免得把 Windows/Linux 桌面版的 target/ 带进包里。
define Build/Prepare
	mkdir -p $(PKG_BUILD_DIR)/src
	$(CP) ./src/. $(PKG_BUILD_DIR)/src/
	rm -rf $(PKG_BUILD_DIR)/src/rust/target
endef

define Build/Compile
	@[ -n "$(RUST_TRIPLE)" ] || { \
		echo "ERROR: 架构 $(ARCH) 推不出对应的 Rust musl target。"; \
		echo "       请在顶层 Makefile 的 RUST_TRIPLE_* 表 / 前缀兜底里补上，"; \
		echo "       或用 RUST_TRIPLE=<triple> 显式指定。"; \
		exit 1; }
	$(MAKE) -C $(PKG_BUILD_DIR)/src \
		ARCH="$(ARCH)" \
		RUST_TRIPLE="$(RUST_TRIPLE)" \
		compile
endef

# 包内含架构相关二进制，不能是 all —— 置空让 package.mk 按板级架构打包。
define Package/ipswitch/install
	$(INSTALL_DIR) $(1)/usr/bin
	$(INSTALL_BIN) \
		$(PKG_BUILD_DIR)/src/target/$(RUST_TRIPLE)/release/ipswitchd \
		$(1)/usr/bin/ipswitchd

	$(INSTALL_DIR) $(1)/etc/config
	$(INSTALL_CONF) ./root/etc/config/ipswitch $(1)/etc/config/ipswitch

	$(INSTALL_DIR) $(1)/etc/init.d
	$(INSTALL_BIN) ./root/etc/init.d/ipswitch $(1)/etc/init.d/ipswitch

	$(INSTALL_DIR) $(1)/etc/uci-defaults
	$(INSTALL_BIN) ./root/etc/uci-defaults/50-ipswitch $(1)/etc/uci-defaults/50-ipswitch
endef

$(eval $(call BuildPackage,ipswitch))
