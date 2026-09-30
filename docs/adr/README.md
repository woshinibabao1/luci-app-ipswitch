# 架构决策记录（ADR）

本目录记录 luci-app-ipswitch 中**已经定案、且改动成本较高**的取舍。
每条 ADR 只讲一件事：当时的约束、考虑过的选项、以及为什么选了这个。

| 编号 | 标题 | 一句话 |
|---|---|---|
| [0001](0001-reuse-at-webserver-rust-channel.md) | 复用 MT5700 Console 的 AT 通道，自己不开串口 | 串口同一时刻只能有一个持有者 |
| [0002](0002-device-side-state-as-completion-criterion.md) | 以模组侧状态判定「切换完成」 | 接口地址会残留，不能单独作判据 |
| [0003](0003-redial-writes-back-current-apn.md) | 重拨前把当前 APN 原样写回 | 省略形态会让模组清空 APN → 断网 |
| [0004](0004-marker-only-on-success.md) | 标记文本只在成功时输出 | 调用方把「看到标记」当作成功的唯一依据 |

新增决策时：编号递增，**不要修改已定案 ADR 的结论**——若要推翻，新开一条并在其中
说明「取代 000X」。
