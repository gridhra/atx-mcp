# atx-mcp

[English](README.md) | [日本語](README.ja.md) | **简体中文**

面向通用 AI agent 的确定性(非生成式)资产变换 MCP 服务器,使用 Rust 编写。

它把编辑意图(例如"把水平线调平、裁成 16:9、稍微调亮一点")作为声明式的变换配方
(recipe)来执行,并将每一次结果都作为不可变(immutable)的修订版本(revision)
进行追踪。原始资产永远不会被修改。

完整设计请参见 [docs/DESIGN.md](docs/DESIGN.md)。

## 安装

无需 Rust 工具链。可任选以下一种方式。

### 1. npx(最简单,推荐)

只要有 Node.js 18+ 即可,无需其他依赖。适配当前平台的原生二进制文件会通过
`optionalDependencies` 自动安装。

```sh
claude mcp add asset-transform -- npx -y atx-mcp --workspace /path/to/asset-workspace
```

也可以直接写入 MCP 客户端的配置文件:

```json
{
  "mcpServers": {
    "asset-transform": {
      "command": "npx",
      "args": ["-y", "atx-mcp", "--workspace", "/path/to/asset-workspace"]
    }
  }
}
```

### 2. 预编译二进制文件

安装脚本(默认安装位置为 `~/.local/bin`,Windows 上为
`%LOCALAPPDATA%\Programs\atx-mcp`;解压前会用 SHA256SUMS 校验):

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/gridhra/atx-mcp/main/scripts/install.sh | sh
```

```powershell
# Windows
irm https://raw.githubusercontent.com/gridhra/atx-mcp/main/scripts/install.ps1 | iex
```

如需手动下载,可从 [Releases](https://github.com/gridhra/atx-mcp/releases) 获取
`atx-mcp-<version>-<target>.tar.gz`(Windows 为 `.zip`)。提供以下目标平台:

| 平台 | 目标三元组 (target triple) |
|---|---|
| macOS (Apple Silicon) | `aarch64-apple-darwin` |
| macOS (Intel) | `x86_64-apple-darwin` |
| Linux x86_64 | `x86_64-unknown-linux-musl`(静态链接,无需 glibc) |
| Linux arm64 | `aarch64-unknown-linux-musl`(静态链接,无需 glibc) |
| Windows x86_64 | `x86_64-pc-windows-msvc` |

```sh
claude mcp add asset-transform -- ~/.local/bin/atx-mcp --workspace /path/to/asset-workspace
```

### 3. 从源码构建(以上平台之外)

只需要 Rust 工具链和 C 编译器(用于构建随附源码的 libwebp)。

```sh
cargo build --release
# => target/release/atx-mcp
claude mcp add asset-transform -- "$PWD/target/release/atx-mcp" --workspace /path/to/asset-workspace
```

---

`--workspace`(环境变量:`ATX_WORKSPACE`)是资产存储所在的目录,若不存在会自动创建。

## 工具(10 个)

| 工具 | 作用 |
|---|---|
| `list_operations` | 配方词汇的精简目录:列出全部操作(op)及其一行说明和参数的类型/取值范围提示,并在末尾附上内置预设名称。可用 `category:"geometry"\|"color"\|"filter"\|"output"` 筛选(只读) |
| `explain_operation` | 单个操作的完整参考:参数表(类型、取值范围、必填/默认值、语义)、可直接粘贴的 JSON 示例以及注意事项。名称无效时会返回全部有效操作名(只读) |
| `import_asset` | 将本地图片导入工作区(基于 sha256,幂等) |
| `inspect_image` | 检查尺寸、EXIF、ICC 配置文件、是否含 GPS 信息等(只读) |
| `detect_tilt` | 通过 Canny+Hough(粗定位)加投影轮廓法(精细化到 0.1° 以内)估算倾斜角度,同时返回水平/垂直族的估计值及其评分曲线。置信度低时返回"不进行校正"(只读) |
| `render_preview` | 以低分辨率(长边 ≤768)应用配方(或 `preset` 预设)并以内联图像返回。可通过 `overlay:"grid"\|"thirds"\|"horizon"` 叠加构图参考线(仅绘制在预览图上,不影响实际变换) |
| `apply_transform` | 以完整分辨率应用配方(或 `preset` 预设)并生成新的修订版本(同一配方 → 同一修订版本) |
| `compare_revisions` | 将两个修订版本缩放到长边 ≤640,通过 `layout:"side_by_side"\|"stacked"` 拼接为一张内联图像返回(用于 A/B 或前后对比的可视化) |
| `list_assets` | 查阅修订版本台账(只读) |
| `export_asset` | 将修订版本导出到指定路径(仅在显式设置 `overwrite:true` 时才会覆盖已存在的文件) |

## 配方示例

```json
{
  "operations": [
    { "op": "rotate", "angle_degrees": -1.8 },
    { "op": "crop", "aspect_ratio": "16:9" },
    { "op": "resize", "width": 1600 },
    { "op": "encode", "format": "webp", "quality": 82 }
  ]
}
```

支持的操作(op):`auto_orient` / `rotate` / `perspective` / `crop`(crop、pad)/
`resize`(cover、contain、fill)/ `adjust` / `color_matrix` / `curves` / `levels` /
`blur` / `median` / `unsharp_mask` / `encode`(jpeg、png、webp、avif)/ `strip_metadata`。
操作清单刻意不写进工具的 schema:请调用 `list_operations` 获取最新目录,
调用 `explain_operation` 获取单个操作的完整 schema、示例与注意事项。

## 预设

`apply_transform` 与 `render_preview` 接受 `recipe`(原始 DSL)或
`preset`(随包提供的具名配方,见 [`presets/`](presets))之一(二者互斥,且必须有其一):

| 预设 | 作用 |
|---|---|
| `eyecatch_16_9` | 居中裁剪为 16:9 → 宽 1600px → WebP q82 |
| `thumbnail_square` | 居中裁剪为 1:1 → 800x800 → WebP q80 |
| `web_optimize` | 不放大地收进 2000x2000 → WebP q80 |
| `grayscale` | 通过 BT.709 亮度 `color_matrix` 转黑白 |
| `sepia` | 通过 `color_matrix` 实现经典棕褐色调 |

预设只是语法糖:解析后作为普通配方走同一条流水线,
`recipe_hash`(幂等键)基于**解析后的配方**计算 ——
因此用预设调用与写出等价的原始配方会落到同一个修订版本。

## 保证

- **确定性**:相同输入 + 相同配方 → 输出字节级完全一致(通过 golden test 做回归验证)
- **幂等性**:配方会先归一化(按键排序 + 对 f64 值按 1e-6 网格量化),再计算 sha256。
  若 `(输入修订版本, 配方哈希)` 相同,则直接返回已存在的修订版本,不会重新生成
- **保护原始资产**:`objects/` 采用仅追加(append-only)的内容寻址存储,不提供
  删除或覆盖的 API

## 开发

```sh
cargo test --workspace     # 单元测试 + 集成测试 + 属性测试(proptest)
cargo clippy --workspace --all-targets -- -D warnings
```

crate 结构:`atx-core`(配方与变换引擎)/ `atx-geometry`(倾斜检测)/
`atx-store`(不可变资产存储)/ `atx-mcp`(rmcp stdio 服务器)。

发布流程请参见 [RELEASING.md](RELEASING.md)。

## 许可证

MIT。详见 [LICENSE](LICENSE)。
