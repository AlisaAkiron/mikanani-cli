# mikan

一个用于下载 [Mikan Project（蜜柑计划）](https://mikanani.me) 番剧 RSS 订阅的交互式命令行工具。

给它一个订阅链接，它便会引导你挑选剧集，并把它们送到你想要的地方——保存为
`.torrent` 文件、导出成纯文本的 URL 列表，或者通过 WebUI API 直接添加到
qBittorrent。同时也提供了完全非交互的模式，方便脚本和定时任务使用。

```
$ mikan "https://mikanani.me/RSS/Bangumi?bangumiId=3950&subgroupid=597"

? Mikan Project - 石纪元 科学与未来 第3部分
> [x] [猎户压制部] 新石纪 第四季 [37] [1080p] [繁日内嵌]  (835.0 MiB, 2026-06-28)
  [ ] [猎户压制部] 新石纪 第四季 [36] [1080p] [繁日内嵌]  (842.1 MiB, 2026-06-21)
  ...
  输入以筛选 · 空格：选中/取消 · →/←：全选/全不选 · 回车：下一步 · Esc：取消
```

## 功能特性

| | |
|---|---|
| **交互式向导** | 多选剧集列表，显示文件大小与发布日期，随后选择一个或多个导出目标。 |
| **三种导出目标** | 下载 `.torrent` 文件、写出以换行分隔的 URL 列表，或直接添加到 qBittorrent。 |
| **qBittorrent 集成** | 保存具名的连接配置，为每个订阅自动创建分类，通过 WebUI API 上传种子。 |
| **非交互模式** | 全程由命令行参数驱动（`--all` / `--latest` / `--filter`）——无需任何提示，适合定时任务。 |
| **代理支持** | 支持 `--proxy`、标准代理环境变量，以及 macOS 系统代理——`mikanani.me` 经常遭到 DNS 污染。 |
| **天然安全** | 剥除订阅文本中的控制字符 / 双向字符 / 零宽字符，清洗文件名，并以仅属主可读写的权限保存配置文件。 |

## 安装

需要较新的 Rust 工具链（edition 2024，Rust 1.85 及以上）。

### 从源码构建

```sh
git clone <本仓库地址> mikanani-cli
cd mikanani-cli
cargo install --path .
```

这会把名为 `mikan` 的可执行文件安装到 `~/.cargo/bin`。

### 使用 Nix

仓库提供了 flake：

```sh
nix run .                # 不安装直接运行
nix build .             # 构建到 ./result
nix develop             # 进入带工具链的开发环境
```

## 使用方法

### 搜索模式

不想手动拼接 RSS 链接时，可以直接搜索：

```sh
mikan                    # 直接进入搜索（会提示输入关键词）
mikan search 石纪元       # 带关键词搜索
```

搜索流程：输入关键词 → 从匹配的番剧中选择一个 → 选择字幕组（仅有一个时自动跳过，
多个时列表顶部提供“全部字幕组”）→ 进入既有的剧集多选与导出流程。搜索为交互模式，
非交互（`-y`）模式仍需显式传入 RSS 链接。

由于蜜柑搜索最多返回约 4 条结果，想要的番剧未必出现在首次搜索里。此时无需退出重来：
在番剧列表底部选择“重新搜索”即可重新输入关键词（会带出上次的输入以便修改）；在字幕组
列表选择“返回上一步”则回到番剧列表。

> 由于 `mikanani.me` 常遭 DNS 污染，搜索同样建议配合 `--proxy` 或系统/环境代理使用。

### 交互式向导

传入一个 Mikan RSS 订阅链接，然后按提示操作：

```sh
mikan "https://mikanani.me/RSS/Bangumi?bangumiId=3950&subgroupid=597"
```

向导最多包含四个步骤：

1. **挑选剧集**——一个多选列表；当所有剧集都带有日期时按最新在前排序（否则沿用订阅自身的顺序）。空格键切换选中，`→`/`←` 全选/全不选，回车继续，Esc 取消。
2. **选择导出格式**——可任意组合*下载 `.torrent` 文件*、*写出 URL 列表*和*添加到 qBittorrent*。
3. **导出路径**——仅当有内容写入磁盘时才询问。会记住你最近使用过的路径，并以历史记录做自动补全（`↑`/`↓` 浏览，Tab 填入），`~` 会被展开为主目录。
4. **qBittorrent 配置与分类**——仅当选择添加到 qBittorrent 时才询问。选取一个已保存的配置（或就地创建一个），然后询问分类（默认为清洗后的订阅标题）。

### qBittorrent 配置

qBittorrent WebUI 的连接信息会以具名配置的形式保存，这样就无需每次运行都重新
输入。用户名留空表示使用 qBittorrent 的“对本机连接跳过身份验证”模式。

```sh
mikan qbt set [名称]        # 交互式创建/更新配置（默认名称为 "default"）
mikan qbt list              # 列出已保存的配置
mikan qbt test [名称]       # 连接并打印 qBittorrent 版本
mikan qbt remove <名称>     # 删除一个配置
```

`qbt set` 和 `qbt test` 都会在保存前先尝试连接，因此地址或凭据的笔误可以被
立即发现。

### 非交互模式

加上 `-y`/`--yes` 即可在无任何提示的情况下运行。此时必须指定**选择什么**以及
**送到哪里**：

```sh
# 最新的 3 集 → 下载 .torrent 文件到 ./torrents
mikan -y --latest 3 --out ./torrents "https://mikanani.me/RSS/Bangumi?bangumiId=3950&subgroupid=597"

# 全部剧集 → 添加到 "default" qBittorrent 配置
mikan -y --all --qbt "https://mikanani.me/RSS/..."

# 仅 1080p 的剧集 → 使用具名配置和自定义分类
mikan -y --filter 1080p --qbt=seedbox --category "Dr. Stone" "https://mikanani.me/RSS/..."

# 组合多个目标：既写出 URL 列表，又下载文件
mikan -y --all --url-list ./lists --out ./torrents "https://mikanani.me/RSS/..."
```

**选择参数**（至少选一个）：

| 参数 | 作用 |
|---|---|
| `--all` | 订阅中的所有剧集。 |
| `--latest N` | 最新的 `N` 集。 |
| `--filter TEXT` | 标题包含 `TEXT` 的剧集（不区分大小写）。可与 `--latest` 组合使用。 |

**输出参数**（至少选一个）：

| 参数 | 作用 |
|---|---|
| `--out DIR` | 将 `.torrent` 文件下载到 `DIR`。 |
| `--url-list DIR` | 将种子 URL 列表写入 `DIR`。 |
| `--qbt[=PROFILE]` | 使用已保存的配置添加到 qBittorrent（裸写 `--qbt` 表示使用 `default`）。 |
| `--category NAME` | qBittorrent 分类（默认为清洗后的订阅标题）。 |

在 `-y` 模式下，qBittorrent 配置是只读的——绝不会创建或保存配置——因此可以放心地
无人值守运行。请先用 `mikan qbt set` 交互式地设置好配置。

### 代理

`mikanani.me` 常因 DNS 污染而无法访问。当抓取因连接 / TLS / 网关错误失败时，工具会
建议使用代理。可通过以下方式提供：

```sh
mikan --proxy http://127.0.0.1:7890 "https://mikanani.me/RSS/..."
```

否则会自动探测代理：

- 标准的 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` 环境变量在**所有**平台上都会被采用。
- 操作系统级别的系统代理会在 **macOS**（网络设置）和 **Windows**（Internet 选项）上被自动读取。Linux 没有系统级的代理设置，因此在 Linux 上环境变量是唯一的自动来源。

注意，与 qBittorrent 的连接会**刻意绕过**代理，因为 WebUI 位于本机/局域网。

## 配置

配置文件 `config.toml` 的位置：

- **Linux / macOS**——`$XDG_CONFIG_HOME/mikanani-cli/`（未设置时回退到 `~/.config/mikanani-cli/`）
- **Windows**——`%APPDATA%\mikanani-cli\`

其中包含：

- **`path_history`**——你最近使用的 10 个导出路径，用于自动补全。
- **`qbt`**——已保存的 qBittorrent 配置（地址、用户名、密码）。

由于该文件可能包含 qBittorrent 密码，它会以原子方式写入，并在 Unix 上设置为仅属主
可读写（`0600`）权限。如果文件某天变得无法解析，它会被移动到 `config.toml.bak` 而
不是被静默覆盖，因此已保存的配置绝不会丢失。

## 退出码

- `0`——成功（或干净地“未选择任何内容”/“已取消”）。
- `1`——已完成，但有一个或多个剧集下载或添加失败。
- `2`——用法错误（参数缺失或无效）。

批处理过程中的失败会逐集报告，绝不会中断整个运行。

## 许可证

基于 [MIT 许可证](LICENSE)发布。
