# ADR 0003：重拨前把当前 APN 原样写回

- 状态：**已采纳**
- 日期：定案于项目初版（v1.0.0）

## 背景

「重新拨号」的实现是：

```
AT^SETAUTODIAL=0                       ← 断开
（等模组确认断开）
AT^SETAUTODIAL=1,<dial_mode>           ← 重新拨号
```

看起来第二步写成短形态（只给 `enable` 和 `dial_mode`）就够了 —— AT 手册里这几个
参数确实是可选的。但真机行为不是这样：

> 手册 16.18 `AT^SETAUTODIAL=<enable>,<dial_mode>[,<protocol>,<apn>,<user>,<passwd>,<authtype>]`
>
> `enable=1` 而**省略 APN** 时，模组会用**空 APN** 去发起拨号 →
> 被网络拒绝 → 进入**退避窗口**。

也就是说，短形态的重拨**会把原本配好的 APN 擦掉**，直接把设备弄成断网。
而用户的场景恰恰是「在弱网/被限流时换 IP」——**在这个时刻断网是最不能接受的**。

（MT5700 Console 项目里有对应记录：`AT^SETAUTODIAL` 与 `+CGDCONT` **不同步**，
空 APN 会让 attach 被拒。参见其项目记忆里关于 APN 配置的条目。）

## 选项

**A. 短形态重拨（`AT^SETAUTODIAL=1,<dial_mode>`）**
- 优点：简单，不需要解析任何东西
- 缺点：**清空 APN → 断网**。不可接受

**B. 让用户把 APN 也填进本服务的配置里，重拨时带上**
- 优点：简单、可预测
- 缺点：`method='redial'` 的语义是「**不动 APN**」，却要求用户再维护一份 APN 配置
  —— 两份配置会漂移，且用户改 APN（在 LuCI 页面里）后本服务不知道，会把旧值写回去

**C. 重拨前先读当前 APN，原样写回** ← 本方案
- 优点：真正「不动 APN」；用户在哪里改的 APN 都能被如实保留
- 缺点：多两条 AT 往返（读 + 回读），且要能解析 `^SETAUTODIAL` 的输出；
  解析失败时要有已知安全的退化路径

## 决策

选 **C**，实现为 `switcher.switch_ip()` 的第 ③ 步：

1. 先发 `AT^SETAUTODIAL?` 读出当前配置（`parse_setautodial()`）
2. 重拨时把读到的 `protocol/apn/username/password/auth_type` **原样拼回去**：
   ```
   AT^SETAUTODIAL=1,<mode>,"<protocol>","<apn>","<user>","<pass>",<auth>
   ```
3. **读不出来才退化**为短形态 `AT^SETAUTODIAL=1,<mode>`
4. 切完**再回读一次**比对：若 APN 与预期不符（被模组拒绝/改写）→ **报错中止**，
   不吐标记

两个细节：

- `parse_setautodial()` 兼容 `^SETAUTODIAL` 与错拼的 `^SETAUTODAIL`
  —— 手册与实际回显存在不一致，不赌哪一个
- 拼接时所有字段都过 `sanitize_at_param()`（白名单 `[A-Za-z0-9._-]`，截断 99 字节）。
  LuCI 前端的 `sanitizeAtParam` **挡不住手写配置文件**，后端必须再过滤一遍，
  否则一个 `"` 就能闭合引号拼出第二条 AT 命令
- `split_csv_fields()` 尊重双引号、**保留空字段** —— 否则空的 user/pass
  会让后面的字段整体左移，把密码写进 auth_type 的位置

## 后果

- ✅ 语义正确：`redial` 真的只重拨，不碰 APN
- ✅ 防住了最坏情况（在这个功能最不该断网的时刻断网）
- ✅ 回读校验让「模组静默拒绝写入」这种情况**变成显式失败**而不是假成功
  （呼应 MT5700 Console 记录过的红线：**AT 回 OK ≠ 写进 NV**，
  `^SYSCFGEX` 就出现过「回 OK 却一字不改」）
- ⚠️ 切换多花两条 AT 往返（真机单条 65–133ms，合计约 0.2s）
  —— 相对整个切换（数十秒）可忽略
- ⚠️ `method='apn'` 时本就带着目标 APN，不需要这条回写逻辑；
  该分支走 `apn_command()`，同样做 sanitize

## 相关

- `src/rust/src/switcher.rs`：`dial_command()`、`parse_setautodial()`、
  `split_csv_fields()`、第 ③/⑦ 步
- `src/rust/src/config.rs`：`sanitize_at_param()`
- 测试：`redial_command_writes_back_current_apn`、
  `redial_command_falls_back_to_short_form_when_unreadable`、
  `csv_split_respects_quotes_and_empty_fields`
