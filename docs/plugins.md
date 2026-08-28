# 数据插件

插件用于把外部数据源抓取到页面变量 `{{PLUGIN.<插件名>.<变量>}}`。

## 放置位置

`plugins/*.json`，每个文件一个插件。查找顺序：exe 同级 `plugins/` 优先，再逐级向上目录。同名插件以先找到的为准。

插件文件在程序启动时读取一次，增删改后需重启程序。

## 数据源

`url` 两种写法：

- HTTP 地址（默认）：周期抓取，支持 `{steam_id}` 占位（自动替换为当前监控的 SteamID），周期最小 60 秒
- `file:<文件名>`：读 exe 同级本地文件（如 `file:data.log`），周期最小 1 秒

## 文件格式

```json
{
  "name": "time",
  "url": "https://timeapi.io/api/Time/current/zone?timeZone=Asia/Shanghai",
  "interval_sec": 60,
  "retry_sec": 30,
  "extract": [
    { "var": "TIME", "type": "regex", "pattern": "\"dateTime\":\"[^\"]*T(\\d{2}:\\d{2}:\\d{2})" },
    { "var": "DATE", "type": "regex", "pattern": "\"dateTime\":\"(\\d{4}-\\d{2}-\\d{2})" }
  ]
}
```

| 字段 | 默认 | 说明 |
|---|---|---|
| `name` | 必填 | 插件名，变量前缀 `{{PLUGIN.<name大写>.变量}}` |
| `url` | 必填 | HTTP 地址或 `file:` 本地文件 |
| `user_agent` | `curl/8.5.0` | 请求 UA（仅 HTTP） |
| `no_proxy` | `true` | 直连不走系统代理（仅 HTTP） |
| `interval_sec` | `1800` | 抓取周期（秒），HTTP 最小 60、文件最小 1 |
| `retry_sec` | `300` | 限流 429 后的重试间隔（秒），最小 30 |
| `extract` | 必填 | 提取规则列表 |
| `mock` | 空 | 调试模式（占位数据）下本插件输出的模拟值：`{ "变量": 值 }`。数字 = 起始值（缓慢递增，便于调试前端动画）；字符串 = 固定输出。未定义时调试模式不产出该插件数据 |

`extract` 每项的字段：

| 字段 | 说明 |
|---|---|
| `var` | 变量名 |
| `type` | `regex` 或 `count` |
| `pattern` | 正则。`regex` 取第一个匹配的第一个捕获组；`count` 返回全部匹配的条数 |

正则语法为 Rust `regex` crate（不支持前后瞻），计数式重复（如 `{0,60000}`）过大会编译失败。

## 数据日志 data.log

程序把获取到的数据写入 exe 同级 `data.log`（每次启动清空，超过 20MB 自动清空），可用 `file:data.log` 插件读取。行格式：

```
[2026-08-28 13:00:00] [ACCOUNT] steam_id=76561198079796198 nickname=xxx
[2026-08-28 13:00:01] [WS] connected
[2026-08-28 13:00:05] [WS] type=10002 data={对局数据原文 JSON: 玩家列表/击杀矩阵/击杀历史/比分}
[2026-08-28 13:01:10] [KILL] self=true killer=<完整steamId> killer_name=xxx weapon=awp victim=<完整steamId> victim_name=xxx
```

- `WS 10002` 同类消息内容变化才写（去重）
- `KILL` 的 `self=` 表示击杀者是否被监控账号；`weapon` 为小写武器码（如 `awp`、`ssg08`、`ak47`）
- 例：统计本次运行内自己某武器击杀数

```json
{ "var": "K", "type": "count", "pattern": "\\[KILL\\] self=true .*weapon=ak47\\b" }
```

## 页面引用

```html
<span>{{PLUGIN.TIME.DATE}} {{PLUGIN.TIME.TIME}}</span>
```

抓到的变量与实时值在 `http://127.0.0.1:8910/all` 的 `plugins` 节点查看。

## 调试

设置窗口底部日志面板输出每个插件的加载、限流与失败原因；`RUST_LOG=debug` 可看每次数据更新。
