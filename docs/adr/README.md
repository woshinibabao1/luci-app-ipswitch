# 架构决策记录（ADR）

本目录记录 luci-app-ipswitch 中**已经定案、且改动成本较高**的取舍。
每条 ADR 只讲一件事：当时的约束、考虑过的选项、以及为什么选了这个。

> **本项目的定位（先记住这句）**：包是**独立**的（独立仓库/独立包/不改 MT5700 Console 一行代码），
> AT 通道是**依赖**的（借道 `127.0.0.1:8765`，没装 Console 就换不了 IP）。
> 这两层答案相反，但说的是不同的事 —— 展开见 [0001](0001-reuse-at-webserver-rust-channel.md) 的「定位澄清」。

| 编号 | 标题 | 一句话 |
|---|---|---|
| [0001](0001-reuse-at-webserver-rust-channel.md) | 复用 MT5700 Console 的 AT 通道，自己不开串口 | 串口同一时刻只能有一个持有者；包独立、通道依赖 |
| [0002](0002-device-side-state-as-completion-criterion.md) | 以模组侧状态判定「切换完成」 | 接口地址会残留，不能单独作判据 |
| [0003](0003-redial-writes-back-current-apn.md) | 重拨前把当前 APN 原样写回 | 省略形态会让模组清空 APN → 断网 |
| [0004](0004-marker-only-on-success.md) | 标记文本只在成功时输出 | 调用方把「看到标记」当作成功的唯一依据 |
| [0005](0005-build-time-hard-constraints.md) | OpenWrt 构建期的三条硬约束 | 顶层禁 `$(error)`／选中包要回读断言／apk 与 ipk 产物名分隔符不同 |
| [0006](0006-resource-bounds-for-untrusted-clients.md) | 面向「不可信客户端」的三处资源上限 | 行读取期上限／写超时／连接数上限；`/health` 要能反映「能不能干活」 |

新增决策时：编号递增，**不要修改已定案 ADR 的结论**——若要推翻，新开一条并在其中
说明「取代 000X」。
