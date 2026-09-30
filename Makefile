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
PKG_BUILD_DIR:=$(BUILD_DIR)/$(PKG_NAME)-$(PKG_VERSION)

include $(INCLUDE_DIR)/package.mk

# ---- OpenWrt ARCH → Rust musl target 映射 ----
# 注意大小端：mips*_24kc 是 big-endian（mips），mipsel* 是 little-endian。
# 这里的映射由 src/Makefile 直接使用（经 RUST_TRIPLE 环境变量传入），
# 以免同一张表在两个文件里各写一份、改一处忘一处。
RUST_TRIPLE_aarch64     := aarch64-unknown-linux-musl
RUST_TRIPLE_x86_64      := x86_64-unknown-linux-musl
RUST_TRIPLE_mipsel      := mipsel-unknown-linux-musl
RUST_TRIPLE_mips        := mips-unknown-linux-musl
RUST_TRIPLE_mips_24kc   := mips-unknown-linux-musl
RUST_TRIPLE_mipsel_24kc := mipsel-unknown-linux-musl
RUST_TRIPLE_arm         := armv7-unknown-linux-musleabihf

RUST_TRIPLE:=$(RUST_TRIPLE_$(ARCH))
ifeq ($(strip $(RUST_TRIPLE)),)
$(error 本包尚不支持架构 $(ARCH)：请在顶层 Makefile 的 RUST_TRIPLE_* 表里补一行)
endif
export RUST_TRIPLE

define Package/ipswitch
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=WWAN
  TITLE:=出口 IP 切换服务（切 APN / 重新拨号）
  URL:=https://github.com/woshinibabao1/luci-app-ipswitch
  # 不写 DEPENDS: 本服务的硬前提是「设备上存在 MT5700 Console 后端」，
  # 但那是个 provides 出来的能力名（at-webserver-rust）。把它写成硬依赖，
  # 一旦构建源的 feed 里没有 luci-app-mt5700，会直接让整个包构建失败 ——
  # 代价远大于收益。改为运行时检查：init.d 启动时记一条明确警告，
  # 切换时若 AT 通道不可用也会给出可读的报错。
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
