# wmpvp-rank-overlay

完美世界竞技平台 CS2 段位监控 OBS 覆盖层（Rust）。

## 编译

需要 [Rust](https://rustup.rs/)。双击 `build.cmd`（或手动执行相同命令）：

```bash
cargo build --release
mkdir -p dist/plugins
cp target/release/wmpvp-rank-overlay.exe dist/
cp -f web/* dist/web/ 2>/dev/null || (mkdir dist/web && cp web/* dist/web/)
cp -f plugins/* dist/plugins/
```

产物在 `dist/`：`wmpvp-rank-overlay.exe` + `web/` + `plugins/`，已有的 `config.json`、`data.log` 与本地插件保留。页面改动后重新运行 `build.cmd` 即刷新；`web.local/`（不入库）存在时会在 `web/` 之后覆盖同名页面，用于本地定制。

## 运行

1. 运行 `dist\wmpvp-rank-overlay.exe`，首次启动打开设置窗口，exe 同级生成 `config.json`
2. 填写 SteamID 与 token，点「保存并应用」即生效
3. OBS 添加浏览器源：

| 源 | URL | 建议尺寸 |
|---|---|---|
| 账号卡 | `http://127.0.0.1:8910/` | 400 × 260 |
| 对局板 | `http://127.0.0.1:8910/match` | 1920 × 1080 |
| 全量数据 | `http://127.0.0.1:8910/all` | 任意 |

端口不同时替换 `8910`。

托盘：左键图标打开设置窗口，右键图标打开菜单（退出）。关闭窗口直接退出。

## config.json

exe 同级，也可在设置窗口修改，保存并应用后生效：

| 字段 | 默认 | 说明 |
|---|---|---|
| `token` | 空 | 完美电竞登录 token。不填：无赛季统计、S 段星级、近五场战绩 |
| `steam_id` | 空 | 被监控玩家的 64 位 SteamID |
| `port` | `8910` | 本地 HTTP 端口 |
| `poll_ms` | `3000` | 对局状态轮询间隔（毫秒） |
| `mock` | `false` | `true` = 占位数据，不访问完美接口（保存后立即切换） |
| `anim_exit` | `true` | 对局板播完入场动画展示 5 秒后倒序退场；`false` = 常驻到下一局 |

token 获取：登录 pvp.wanmei.com，浏览器 F12 → 网络 → 任一 `api.wmpvp.com` 请求头里的 `token` 字段。

## 自定义前端

页面源码在 `web/` 目录，直接编辑，保存后在 OBS 刷新浏览器源即生效：

```
web/
├── account.html   账号卡
├── account.css    存在即自动注入（优先级更高）
├── match.html     对局板
├── match.css      同上
└── all.html       全量数据示例页
```

页面内用变量引用数据：

- `{{MY.NAME}}` —— 文本替换
- `{{{MY.RANKBOX}}}` —— HTML 片段替换

全部变量与实时值在 `http://127.0.0.1:8910/all` 查看（`/api/state` 的完整 JSON）。

常用变量：`{{MY.RANK}}`、`{{MY.RATING}}`、`{{MATCH.CT.AVG}}`、`{{CT.1.NAME}}`（1-5 号位）、`{{{CT.1.BADGE}}}`、`{{{CT.1.FORM}}}`（近五场 W/L 着色块）。

## 数据插件

外部数据以 `{{PLUGIN.<插件名>.<变量>}}` 展示到页面。示例插件 `plugins/time.json`（北京时间，显示在账号卡底部）。运行数据（WS 流、击杀事件、账号）写入 exe 同级 `data.log`，插件可直接读取。用法见 [docs/plugins.md](docs/plugins.md)。

## 目录结构

```
src/
├── main.rs     入口: 设置窗口、托盘、日志、数据源切换
├── monitor.rs  WS 实时监控
├── wmpvp.rs    完美接口
├── form.rs     对局玩家数据抓取
├── rank.rs     段位计算
├── server.rs   本地 HTTP 服务
├── state.rs    共享状态与 JSON 快照
├── plugin.rs   数据插件
├── mock.rs     占位数据
└── config.rs   配置读写
web/            页面源码
assets/         官方段位素材
plugins/        示例插件
```
