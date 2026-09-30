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

# ---- OpenWrt ARCH → Rust musl target 映射（RUST_TRIPLE 由外部传入时优先）----
RUST_TARGET_aarch64     := aarch64-unknown-linux-musl
RUST_TARGET_x86_64      := x86_64-unknown-linux-musl
RUST_TARGET_mipsel      := mipsel-unknown-linux-musl
RUST_TARGET_mips        := mips-unknown-linux-musl
RUST_TARGET_mips_24kc   := mips-unknown-linux-musl
RUST_TARGET_mipsel_24kc := mipsel-unknown-linux-musl
RUST_TARGET_arm         := armv7-unknown-linux-musleabihf

RUST_TARGET := $(if $(RUST_TRIPLE),$(RUST_TRIPLE),$(RUST_TARGET_$(ARCH)))
ifeq ($(strip $(RUST_TARGET)),)
# 兜底：按宿主架构推断，仅用于本地验证
UNAME_M := $(shell uname -m)
RUST_TARGET := $(if $(filter x86_64,$(UNAME_M)),x86_64-unknown-linux-musl,$(if $(filter aarch64 arm64,$(UNAME_M)),aarch64-unknown-linux-musl,))
endif
ifeq ($(strip $(RUST_TARGET)),)
$(error 无法推断 Rust 目标三元组，请通过 RUST_TRIPLE= 环境变量指定)
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
