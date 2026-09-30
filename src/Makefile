# ============================================================================
# Rust 交叉编译子 Makefile
# ----------------------------------------------------------------------------
# 由顶层 Makefile 的 Build/Compile 调用，运行在 $(PKG_BUILD_DIR)/src/ 下。
#
# 链路：
#   顶层 Build/Compile  →  本文件 compile  →  cargo build --target <musl>
#   顶层 Package/install  →  直接取 target/<triple>/release/ipswitchd
#
# RUST_TRIPLE 通常由顶层导出；单独执行本文件时会按 ARCH 自行推断
# （方便在 SDK 里手搓验证，不必先跑整套包构建）。
# ============================================================================

RUST_DIR := rust

# ---- OpenWrt ARCH → Rust musl target 映射 ----
# 注意大小端：mips*_24kc 是 big-endian（mips），mipsel* 是 little-endian。
#
# 本文件只被顶层 Makefile 的 Build/Compile 调用，**不参与包元数据扫描**，
# 所以这里用 $(error) 是安全的（不像顶层 Makefile，那里用 $(error) 会让包
# 直接从 metadata 里消失、还看不到任何错误）。
#
# 正常情况下 RUST_TRIPLE 已由顶层 Makefile 传入，这里的表只是"单独跑本文件"
# 时的兜底。
RUST_TARGET_aarch64_cortex-a53 := aarch64-unknown-linux-musl
RUST_TARGET_aarch64_cortex-a55 := aarch64-unknown-linux-musl
RUST_TARGET_aarch64_generic    := aarch64-unknown-linux-musl
RUST_TARGET_x86_64             := x86_64-unknown-linux-musl
RUST_TARGET_mipsel_24kc        := mipsel-unknown-linux-musl
RUST_TARGET_mips_24kc          := mips-unknown-linux-musl
RUST_TARGET_arm_cortex-a7      := armv7-unknown-linux-musleabihf
RUST_TARGET_arm_cortex-a9      := armv7-unknown-linux-musleabihf

# 优先级：RUST_TRIPLE（外部显式传入）> ARCH 精确表 > ARCH 前缀 > 宿主推断
RUST_TARGET := $(if $(RUST_TRIPLE),$(RUST_TRIPLE),$(RUST_TARGET_$(ARCH)))

ifeq ($(strip $(RUST_TARGET)),)
  # 前缀兜底。★ 顺序要紧：`mipsel%` 必须在 `mips%` 之前，
  # 否则 mipsel_24kc 会被 mips% 抢走、编出大端二进制。★ 但**最后一列是 x86_64% 而不是 x86%**，同理 arm 在前。
  ifneq ($(filter aarch64%,$(ARCH)),)
    RUST_TARGET := aarch64-unknown-linux-musl
  else ifneq ($(filter mipsel%,$(ARCH)),)
    RUST_TARGET := mipsel-unknown-linux-musl
  else ifneq ($(filter mips%,$(ARCH)),)
    RUST_TARGET := mips-unknown-linux-musl
  else ifneq ($(filter arm%,$(ARCH)),)
    RUST_TARGET := armv7-unknown-linux-musleabihf
  else ifneq ($(filter x86_64%,$(ARCH)),)
    RUST_TARGET := x86_64-unknown-linux-musl
  endif
endif

ifeq ($(strip $(RUST_TARGET)),)
  # 只有在**完全没给 ARCH**（本地 `make -C src` 手搓验证）时，才按宿主推断。
  # ★ ARCH 给了却认不出来时**不猜** —— 猜错会静默编出别的架构的二进制，
  #   那种错误要到设备上跑不起来才发现。
  ifeq ($(strip $(ARCH)),)
    UNAME_M := $(shell uname -m)
    RUST_TARGET := $(if $(filter x86_64,$(UNAME_M)),x86_64-unknown-linux-musl,$(if $(filter aarch64 arm64,$(UNAME_M)),aarch64-unknown-linux-musl,))
  endif
endif
ifeq ($(strip $(RUST_TARGET)),)
$(error 架构 $(ARCH) 推不出 Rust 目标三元组，请通过 RUST_TRIPLE= 环境变量指定)
endif

CARGO_TARGET_DIR := $(CURDIR)/target
BIN := $(CARGO_TARGET_DIR)/$(RUST_TARGET)/release/ipswitchd

.PHONY: all compile clean install print-bin

all: compile

compile:
	@echo "==> [ipswitch] cargo build: ARCH=$(ARCH) RUST_TARGET=$(RUST_TARGET)"
	cd $(RUST_DIR) && \
		CARGO_TARGET_DIR="$(CARGO_TARGET_DIR)" \
		cargo build --release --target $(RUST_TARGET)
	test -x "$(BIN)" || { echo "ERROR: cargo 未产出 $(BIN)"; exit 1; }
	@echo "==> [ipswitch] 后端二进制就绪: $(BIN)"

# 独立安装入口：不经过包管理，直接把二进制放到 DESTDIR 下
# （`make install DESTDIR=/tmp/x`），便于先在设备上手工验证。
install: compile
	mkdir -p "$(DESTDIR)/usr/bin"
	cp "$(BIN)" "$(DESTDIR)/usr/bin/ipswitchd"
	chmod 0755 "$(DESTDIR)/usr/bin/ipswitchd"

clean:
	cd $(RUST_DIR) && cargo clean 2>/dev/null || true
	rm -rf "$(CARGO_TARGET_DIR)"
