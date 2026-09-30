# ADR 0005：OpenWrt 构建期的三条硬约束

- 状态：**已采纳**
- 日期：2026-09-30（首次 CI 打通时定案）

## 背景

本包在 GitHub Actions 里连失败三轮才出包。三次的**根因各不相同**，
但有一个共同点：**make 全都返回 0 或只给一句与真因无关的报错**。
也就是说，这三条都是"静默失败"——不写成约束，下次一定重踩。

## 约束一：顶层 Makefile 里**禁止** `$(error)`

### 现象

CI 里 `make package/ipswitch/compile` 什么都没编就"成功"了，
`.config` 里连 `# CONFIG_PACKAGE_ipswitch is not set` 都不存在，
而且**整份日志里没有一句提到 ipswitch 的错误**。

### 原因

OpenWrt 生成 `tmp/.config-package.in` 时，会对**每个包**跑一次

```
make -C package/<name> DUMP=1
```

来收集元数据。我原来在顶层 Makefile 里写了：

```make
RUST_TRIPLE:=$(RUST_TRIPLE_$(ARCH))
ifeq ($(strip $(RUST_TRIPLE)),)
$(error 本包尚不支持架构 $(ARCH)：请在顶层 Makefile 的 RUST_TRIPLE_* 表里补一行)
endif
```

而元数据扫描阶段 `ARCH` 的取值与正式构建阶段**不同**（扫描时表里查不到），
于是解析期 `$(error)` 触发、那次子 make 失败。

失败链是这样的：

```
子 make 失败 → 包不进 metadata → PACKAGE_ipswitch 在 kconfig 里不存在
  → defconfig 把 CONFIG_PACKAGE_ipswitch 当未知符号丢掉（不留痕迹）
  → package-ipkg.mk 里这句为假：
        ifneq ($(CONFIG_PACKAGE_$(1))$(DEVELOPER),)
        compile: ...     ← 只有非空时才给 compile 挂依赖
  → compile 成了没有任何依赖的空目标，清理一下就返回 0
```

而 `$(error)` 的消息**透不出来**（子 make 的输出被吞掉），所以现场只有一句
"包凭空消失了"，指向完全错误的方向。

### 决定

**解析期一律给安全值，报错推迟到真正构建时。**

- 顶层 Makefile：不做任何 `$(error)`；查不到就留空
- 真正报错放在 `Build/Compile` 里显式检查并 `exit 1`

`src/Makefile` 不受此限（它只被 `Build/Compile` 调用，不参与元数据扫描），
那里用 `$(error)` 是安全的。

### 附带教训

**判断"包是否被选中"不能用 `grep '^CONFIG_PACKAGE_x='` 这种间接证据，
要直接看 `.config` 里有没有这一项。** 「连 `is not set` 都没有」这个细节，
是唯一能区分"没被选中"和"符号根本不存在"的线索。

## 约束二：SDK 里选中包之后**必须回读断言**

### 现象

包没被选中时，`make package/<pkg>/compile` **返回 0**。
脚本会一路跑到最后，靠"产物目录里没有包"才发现 —— 一次白等好几分钟。

### 决定

照搬 MT5700 Console 里已经跑通的写法，四个细节一个都不能省：

```sh
touch .config                                   # ① SDK 解压后不保证有 .config
grep -v '^CONFIG_PACKAGE_ipswitch=' .config > .config.new || true
mv .config.new .config                          # ② 先删同名旧行再追加
echo 'CONFIG_PACKAGE_ipswitch=m' >>.config      # ③ =m（SDK 只编包，=y 是"进固件"）
make defconfig >/dev/null
grep -E 'CONFIG_PACKAGE_ipswitch' .config || echo "    (一条都没有)"   # 诊断
grep -qE '^CONFIG_PACKAGE_ipswitch=[my]$' .config || {
	echo "ERROR: ipswitch 未被 .config 选中 —— 继续编译只会得到空包"
	exit 1                                       # ④ ★ 唯一能挡住假绿的地方
}
```

第 ④ 条是**唯一**能在"编译之前"就挡住假绿的地方。前三条是让它真的能选中。
另外那条 `grep ... || echo "(一条都没有)"` 是诊断输出 —— 它正是第三轮定位
根因的关键（`.config` 里什么都没有 ⇒ 符号不存在，而不是"没被选中"）。

## 约束三：两种包格式的产物名**分隔符不一样**

### 现象

`23.05.5`（ipk）那条 job 全绿，`main`（SNAPSHOT，apk）却报"没有产出包"，
而**日志里明明能看到包已经编出来了**：

```
bin/packages/aarch64_cortex-a53/base/ipswitch-1.0.0-r1.apk
```

### 原因

```
ipk:  ipswitch_1.0.0-1_aarch64_cortex-a53.ipk   ← 包名与版本之间是【下划线】
apk:  ipswitch-1.0.0-r1.apk                    ← 是【连字符】
```

只写 `ipswitch_*.apk` 就永远匹配不到 apk 格式的产物。

### 决定

收集与校验一律用 `ipswitch[-_]*`（workflow 里也是）：

```sh
find bin -type f \( -name 'ipswitch[-_]*.apk' -o -name 'ipswitch[-_]*.ipk' \)
```

### 附带教训

"没有产出包"这类报错**必须把实际找到了什么打出来**（`find bin -type f | head -30`）。
我脚本里原本就有这一行，正是它把 `ipswitch-1.0.0-r1.apk` 摆到了眼前 ——
否则还会继续往"编译没跑起来"的方向查。

## 相关

- `Makefile`：约束一（架构映射段，注释里也标了"禁 `$(error)`"）
- `src/Makefile`：约束一的例外说明
- `scripts/sdk-build.sh`：约束二、三
- `.github/workflows/build.yml`：约束三（`校验产物存在`）
- MT5700 Console 的 `scripts/sdk-build.sh`：约束二的来源（已跑通的配方）
