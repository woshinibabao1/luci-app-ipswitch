#!/bin/sh
# ============================================================================
# 在 OpenWrt SDK 容器里交叉编译 ipswitch，并打出 .apk / .ipk。
#
# 用法: sdk-build.sh <ARCH> <RUST_TRIPLE> <VER> [TARGET_DIR]
#   ARCH        OpenWrt 架构名（aarch64_cortex-a53 / x86_64 / mips_24kc ...）
#   RUST_TRIPLE Rust musl 目标三元组（aarch64-unknown-linux-musl ...）
#   VER         OpenWrt 版本（main / 23.05.5 ...）
#   TARGET_DIR  SDK target 目录，默认 mediatek/filogic（H5000M 所属）
#
# 产物落在 /out（调用方把 out 目录挂进来）。
#
# ── 交叉编译的两个关键点 ────────────────────────────────────────────────────
#   1) **用 zig 当链接器**。Rust 官方 musl target 自带 crt 与 lld，但认不出
#      OpenWrt 注入的链接旗标（aarch64_cortex-a53 会带 --fix-cortex-a53-843419），
#      会直接链接失败。zig cc 能吃下这些旗标。
#   2) **cargo 配置里必须写 `link-self-contained=no`**。否则 zig 与 rustc
#      各带一份 crt，链接期报 duplicate symbol: _start/_init/_fini。
#      这两条是 MT5700 Console 已经在用的配方，原样复用而不是重新试错。
#
#   Rust 工具链与 zig 只装进容器的 /opt，不写入仓库。
# ============================================================================
set -e

ARCH="$1"
RUST_TRIPLE="$2"
VER="$3"
TARGET_DIR="${4:-mediatek/filogic}"

[ -n "$ARCH" ] && [ -n "$RUST_TRIPLE" ] && [ -n "$VER" ] || {
	echo "usage: sdk-build.sh <ARCH> <RUST_TRIPLE> <VER> [TARGET_DIR]"
	exit 1
}

echo "==> 开始: arch=$ARCH triple=$RUST_TRIPLE ver=$VER target=$TARGET_DIR"

# ---------- 0) 基础工具 ----------
if command -v apt-get >/dev/null 2>&1; then
	SUDO=''
	[ "$(id -u)" -ne 0 ] && SUDO='sudo'
	$SUDO apt-get update -qq -o Acquire::Check-Valid-Until=false >/dev/null 2>&1 || true
	$SUDO apt-get install -y -qq xz-utils zstd >/dev/null 2>&1 || true
fi
command -v curl >/dev/null 2>&1 || {
	echo "ERROR: 容器缺少 curl"
	exit 1
}

# ---------- 1) 下载并解压 OpenWrt SDK ----------
case "$VER" in
main | snapshots) BASE_URL="https://downloads.openwrt.org/snapshots" ;;
*) BASE_URL="https://downloads.openwrt.org/releases/$VER" ;;
esac
mkdir -p /builder
echo "==> 定位 SDK: $BASE_URL/targets/$TARGET_DIR/"
LISTING=$(curl -sL "$BASE_URL/targets/$TARGET_DIR/")
SDK_FILE=$(echo "$LISTING" | grep -oE 'openwrt-sdk-[^"< ]+\.tar\.(xz|zst)' | grep -v '\.asc' | head -1)
[ -n "$SDK_FILE" ] || {
	echo "ERROR: 未找到 SDK 包（$BASE_URL/targets/$TARGET_DIR/）"
	exit 1
}
echo "==> 下载 SDK: $SDK_FILE"
curl -sSL "$BASE_URL/targets/$TARGET_DIR/$SDK_FILE" -o /builder/sdk.tar
tar -xf /builder/sdk.tar -C /builder
rm -f /builder/sdk.tar
SDK_DIR=$(find /builder -maxdepth 1 -type d -name 'openwrt-sdk-*' | head -1)
[ -n "$SDK_DIR" ] && [ -f "$SDK_DIR/feeds.conf.default" ] || {
	echo "ERROR: SDK 解压异常"
	ls -la /builder
	exit 1
}
cd "$SDK_DIR"
echo "==> SDK 根目录: $SDK_DIR"

# ---------- 2) Rust 工具链 ----------
export RUSTUP_HOME=/opt/rust
export CARGO_HOME=/opt/cargo
if [ ! -x "$CARGO_HOME/bin/rustup" ]; then
	curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null
fi
export PATH="$CARGO_HOME/bin:$PATH"
if [ "$RUST_TRIPLE" = "mipsel-unknown-linux-musl" ]; then
	# mipsel-unknown-linux-musl 自 Rust 1.75 起没有预编译 std，只能 nightly 现编
	echo "==> mipsel: nightly + build-std"
	rustup toolchain install nightly --profile minimal --component rust-src >/dev/null 2>&1
	rustup default nightly
	export CARGO_BUILD_STD_FLAGS="-Z build-std=std,panic_abort"
else
	rustup default stable
	rustup target add "$RUST_TRIPLE"
fi

# ---------- 3) zig（交叉链接器） ----------
ZIG_VER=0.13.0
if [ ! -x /opt/bin/zig ]; then
	mkdir -p /opt/bin
	curl -fsSL "https://ziglang.org/download/${ZIG_VER}/zig-linux-x86_64-${ZIG_VER}.tar.xz" | tar -xJ -C /opt
	ln -sf "/opt/zig-linux-x86_64-${ZIG_VER}/zig" /opt/bin/zig
fi
export PATH="/opt/bin:$PATH"
zig version

case "$RUST_TRIPLE" in
x86_64-unknown-linux-musl) ZIG_TARGET=x86_64-linux-musl ;;
aarch64-unknown-linux-musl) ZIG_TARGET=aarch64-linux-musl ;;
mips-unknown-linux-musl) ZIG_TARGET=mips-linux-musl ;;
mipsel-unknown-linux-musl) ZIG_TARGET=mipsel-linux-musl ;;
armv7-unknown-linux-musleabihf) ZIG_TARGET=arm-linux-musleabihf ;;
*) ZIG_TARGET="$(echo "$RUST_TRIPLE" | sed 's/-unknown-/-/')" ;;
esac

# zig 不认 OpenWrt/LLD 的专有旗标，包一层把它丢掉
cat > /opt/zig-linker <<EOF
#!/bin/sh
for a in "\$@"; do
  case "\$a" in
    *--fix-cortex-a53*) ;;
    *) set -- "\$@" "\$a" ;;
  esac
  shift
done
exec /opt/bin/zig cc -target ${ZIG_TARGET} "\$@"
EOF
chmod +x /opt/zig-linker
sh -n /opt/zig-linker

mkdir -p "${CARGO_HOME}"
cat > "${CARGO_HOME}/config.toml" <<EOF
[target.${RUST_TRIPLE}]
linker = "/opt/zig-linker"
rustflags = ["-C", "link-self-contained=no"]
EOF
echo "==> zig linker: ${RUST_TRIPLE} -> ${ZIG_TARGET}"

# ---------- 4) 把包放进 buildroot 的 package/ ----------
rm -rf package/ipswitch
mkdir -p package/ipswitch
cp -r /work/Makefile /work/root /work/src package/ipswitch/
# cp -r 在部分环境会丢执行位，而 init.d / uci-defaults 少了 +x 会静默失效
chmod 0755 package/ipswitch/root/etc/init.d/ipswitch
chmod 0755 package/ipswitch/root/etc/uci-defaults/50-ipswitch
rm -rf package/ipswitch/src/rust/target

# ---------- 5) 选中并编译 ----------
# ★ 必须显式选中包，而且**选完要回读断言**。
#
# SDK 的 defconfig 不会自动选中"刚复制进 package/"的包；包未被选中时，
# `make package/<pkg>/compile` 只会跑一遍清理、什么都不编、**返回 0**
# —— 也就是产出一个"成功但没有包"的假绿。
#
# 下面这套流程照搬 MT5700 Console 里已经跑通的写法，四个细节都不是多余的：
#   ① `touch .config`  —— SDK 解压后不保证存在 .config；
#      缺了它 grep 与 kconfig 的行为都会变。
#   ② 先 `grep -v` 删掉同名旧行再追加 —— 同一个 symbol 在 .config 里
#      出现两次时 kconfig 取哪一行并不直观。
#   ③ `=m` 而不是 `=y` —— SDK 只负责编包；`=y` 是"装进固件"的语义。
#   ④ defconfig 之后**断言真的被选中** —— 这是唯一能挡住假绿的地方。
#
# ★ RUST_TRIPLE 在这里是**权威来源**：顶层 Makefile 只做 ARCH→triple 的
#   兜底推断，显式传入的值优先（`RUST_TRIPLE ?=`）。放在 defconfig 之前
#   export，保证元数据扫描 `make -C package/ipswitch DUMP=1` 也看得到它。
export RUST_TRIPLE

touch .config
grep -v '^CONFIG_PACKAGE_ipswitch=' .config > .config.new || true
mv .config.new .config
echo 'CONFIG_PACKAGE_ipswitch=m' >>.config
make defconfig >/dev/null

echo "==> .config 里的 ipswitch 相关项（诊断用）："
grep -E 'CONFIG_PACKAGE_ipswitch' .config || echo "    (一条都没有)"

grep -qE '^CONFIG_PACKAGE_ipswitch=[my]$' .config || {
	echo "ERROR: ipswitch 未被 .config 选中 —— 继续编译只会得到空包"
	exit 1
}

echo "==> 编译 ipswitch（target=$RUST_TRIPLE）"
make package/ipswitch/compile V=s

# ---------- 6) 收集产物 ----------
# ★ 两种包格式的产物名**分隔符不一样**，pattern 必须都覆盖：
#     ipk: ipswitch_1.0.0-1_aarch64_cortex-a53.ipk   ← 包名与版本之间用下划线
#     apk: ipswitch-1.0.0-r1.apk                    ← 用连字符
#   只写 `ipswitch_*.apk` 会漏掉 apk —— main(SNAPSHOT) 那条 job 就是这么
#   在"包其实已经编出来"的情况下报"没产出包"的。
mkdir -p /out/"$ARCH"
FOUND=0
for f in $(find bin -type f \( -name 'ipswitch[-_]*.apk' -o -name 'ipswitch[-_]*.ipk' \) 2>/dev/null); do
	cp -v "$f" /out/"$ARCH"/
	FOUND=1
done
if [ "$FOUND" -ne 1 ]; then
	echo "ERROR: 没有产出 ipswitch 包 —— 编译可能没真正执行"
	find bin -type f 2>/dev/null | head -30
	exit 1
fi

# 顺带确认二进制真的进了包（体积闸门：Rust 静态二进制应在数百 KB 量级）
for p in /out/"$ARCH"/ipswitch*; do
	echo "==> 产物: $p ($(wc -c <"$p") bytes)"
done
echo "==> 完成"
