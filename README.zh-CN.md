# atx-mcp

[English](README.md) | [日本語](README.ja.md) | **简体中文**

面向通用 AI agent 的确定性(非生成式)资产变换 MCP 服务器,使用 Rust 编写。

它把编辑意图(例如"把水平线调平、裁成 16:9、稍微调亮一点")作为声明式的变换配方
(recipe)来执行,并将每一次结果都作为不可变(immutable)的修订版本(revision)
进行追踪。原始资产永远不会被修改。

完整设计请参见 [docs/DESIGN.md](docs/DESIGN.md)。

## 使用场景

1. **制作文章题图**
   > "把这张照片扶正,裁成 16:9 的 1600px 题图,导出 WebP。"
   `import_asset` → `detect_tilt`(接近水平时 AI 会判断无需校正)→ `apply_transform`(rotate → crop → resize → encode)→ `export_asset`。原始文件不会被修改,同一配方随时可复现同一结果。

2. **社交/CMS 多尺寸分发**
   > "从这张照片生成 OGP、Instagram 方图和缩略图。"
   从同一张原图并行生成 OGP 1200×630、Instagram 1080 正方形、400px 缩略图。同一配方 = 同一修订版本的幂等性,确保重复执行也不会重复生成;只写一个预设名也能跑。

3. **发布前的安全处理**
   > "务必去掉定位信息,但颜色不要动。"
   `strip_metadata`(`exif`)会移除包含 GPS 的 EXIF,同时保留 ICC 配置文件。AI 也可以先通过 `inspect_image` 的 `has_gps` 提前预警。

4. **色调与风格调整**
   > "只把天空的蓝调深一点,其他不要变。"
   覆盖 `curves` / `levels` / `hsl` / `white_balance`、`film_soft` 预设,以及导入自有 `.cube` LUT(`import_asset` 后用 `lut` 应用)。

5. **局部调整**
   > "天空稍微压暗一点,地面保持不变。"
   `generate_mask` 生成渐变/亮度区间/色相区间蒙版,接入调整后先用 `render_preview` 的 `overlay:"mask"` 目视确认作用范围,再正式应用。

6. **图层合成**
   > "把这张照片模糊一份,以 screen 50% 叠加上去,做出柔焦效果。"
   `layers` 图层栈组合 16 种混合模式、不透明度与蒙版,搭建可复现的柔焦一类合成效果。

7. **水印、修复与透视校正**
   > "在右下角烧录 Logo,去掉电线,再修一下透视变形。"
   `svg_overlay` 烧录 Logo,`clone`/`heal` 通过合成质感与色调去除污点或电线,`perspective` 校正会聚的竖线。

8. **核验与可追责**
   > "把这张图修改前后并排给我看看。"
   `compare_revisions` 并排展示前后对比,或返回像素差异热力图及 `mean_abs_diff` 等统计值。每个修订版本都保留完整谱系,文章所用图片的加工历史可被完整追踪与复现——在任何机器上都是字节级一致。

atx 不做的事(生成式编辑、RAW 显影、基于机器学习的自动裁切等)不在范围内,路线图参见 [docs/DESIGN.md](docs/DESIGN.md)。

## 安装

无需 Rust 工具链。可任选以下一种方式。

### 1. npx(最简单,推荐)

只要有 Node.js 18+ 即可,无需其他依赖。适配当前平台的原生二进制文件会通过
`optionalDependencies` 自动安装。

```sh
# --scope user 使其在所有项目中可用(省略则仅限当前项目)
claude mcp add --scope user asset-transform -- npx -y atx-mcp --workspace /path/to/asset-workspace
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

## 工具(11 个)

| 工具 | 作用 |
|---|---|
| `list_operations` | 配方词汇的精简目录:列出全部操作(op)及其一行说明和参数的类型/取值范围提示,并在末尾附上内置预设名称。可用 `category:"geometry"\|"color"\|"filter"\|"output"` 筛选(只读) |
| `explain_operation` | 单个操作的完整参考:参数表(类型、取值范围、必填/默认值、语义)、可直接粘贴的 JSON 示例以及注意事项。名称无效时会返回全部有效操作名(只读) |
| `import_asset` | 将本地图片导入工作区(基于 sha256,幂等) |
| `inspect_image` | 检查尺寸、EXIF、ICC 配置文件、是否含 GPS 信息等(只读) |
| `detect_tilt` | 通过 Canny+Hough(粗定位)加投影轮廓法(精细化到 0.1° 以内)估算倾斜角度,同时返回水平/垂直族的估计值及其评分曲线。置信度低时返回"不进行校正"(只读) |
| `generate_mask` | 确定性地生成灰度蒙版(`linear_gradient` / `radial_gradient` / `luminosity_range` / `color_range`),存为与参考图像同尺寸的 PNG 修订版本,供操作的 `mask` 字段引用(幂等) |
| `render_preview` | 以低分辨率(长边 ≤768)应用配方(或 `preset` 预设)并以内联图像返回。可通过 `overlay:"grid"\|"thirds"\|"horizon"` 叠加构图参考线,或通过 `overlay:"mask"`(配合 `mask_revision_id`)叠加蒙版覆盖范围(仅绘制在预览图上,不影响实际变换) |
| `apply_transform` | 以完整分辨率应用配方(或 `preset` 预设)并生成新的修订版本(同一配方 → 同一修订版本) |
| `compare_revisions` | 将两个修订版本缩放到长边 ≤640,通过 `layout:"side_by_side"\|"stacked"` 拼接为一张内联图像返回(用于 A/B 或前后对比的可视化);`layout:"diff"` 则返回单张像素差异热力图,并附带 `mean_abs_diff`/`max_abs_diff`/`changed_pixel_ratio` 统计值(要求两者尺寸完全一致) |
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

支持的操作(op,共 21 个):`auto_orient` / `rotate` / `perspective` / `crop`(crop、pad)/
`resize`(cover、contain、fill)/ `adjust` / `color_matrix` / `curves` / `levels` /
`lut` / `white_balance` / `hsl` / `blur` / `median` / `unsharp_mask` / `convolve` /
`clone` / `heal` / `svg_overlay` / `encode`(jpeg、png、webp、avif)/ `strip_metadata`。
操作清单刻意不写进工具的 schema:请调用 `list_operations` 获取最新目录,
调用 `explain_operation` 获取单个操作的完整 schema、示例与注意事项。

### LUT(.cube)

`.cube` 3D/1D LUT 是**素材**而非图像:先导入,再让配方引用它生成的修订版本。

1. 对 `.cube` 文件调用 `import_asset`。它会以 `mime_type: "application/x-cube"`
   存为不可变修订版本(它不是图像,因此 `inspect_image` 会有意返回结构化错误)。
2. 在配方中引用返回的 `revision_id`:

```json
{ "op": "lut", "lut_revision_id": "rev_...", "strength": 0.8 }
```

`strength`(0..1,默认 1.0)与原图线性混合。由于修订版本不可变,把被引用的 id
计入 `recipe_hash` 即可保持完全确定性;但这也意味着该配方只能在持有该 LUT 的
工作区内复现,跨机器搬运风格时请连同 `.cube` 一起搬。引用不存在的 id 会在任何
像素处理开始之前以结构化错误返回。

### SVG 叠加(Logo 与水印)

`.svg` 与 `.cube` LUT 一样是**矢量素材**而非图像:先导入,再让配方引用它生成的
修订版本,把它烧录到栅格图像上。

1. 对 `.svg` 文件调用 `import_asset`。它会以 `mime_type: "image/svg+xml"` 存为
   不可变修订版本,摘要中会给出该 SVG 的**固有尺寸**(`0x0` 表示没有固有尺寸,
   即根 `<svg>` 上既无 `viewBox` 也无绝对的 `width`/`height`)。它不是栅格图像,
   因此 `inspect_image` 会有意返回结构化错误。
2. 在配方中引用返回的 `revision_id`:

```json
{ "op": "svg_overlay", "svg_revision_id": "rev_...",
  "x": 24, "y": 24, "width": 320, "opacity": 0.25, "blend_mode": "normal" }
```

`x` / `y` 是叠加层**左上角**的坐标,按**流水线到此为止**的图像坐标系解释
(所以请把叠加放在 resize / crop 之后);允许为负值,超出画面的部分会被裁掉。
`width` / `height` 都省略则按 SVG 的固有尺寸栅格化,只给一个则按固有宽高比缩放,
两个都给则拉伸到精确尺寸——没有固有尺寸的 SVG,若不同时给出两者会返回结构化错误。
合成公式与 16 种 `blend_mode` 与[图层](#图层)完全一致。

> **文本永远不会被渲染。** 为保证确定性,atx 不加载任何系统字体(各机器安装的
> 字体不同,会破坏逐字节可复现性)。含 `<text>` 的 SVG 只渲染图形、不渲染字形,
> 并返回一条警告。**请在导入前于矢量编辑器中把文本转换为路径(轮廓)**,
> 这样在任何机器上得到的像素都完全相同。

### 蒙版(局部调整)

蒙版是一个**灰度图像修订版本**:其 BT.709 亮度即权重,白色表示该操作全量生效,
黑色表示该像素保持不变。11 个色调/滤镜类操作(`adjust` / `color_matrix` /
`curves` / `levels` / `hsl` / `lut` / `white_balance` / `blur` / `median` /
`unsharp_mask` / `convolve`)都可以接受蒙版。

1. `generate_mask` 会基于参考图像确定性地生成一张**尺寸完全一致**的蒙版:

| `kind` | 参数 | 选中的区域 |
|---|---|---|
| `linear_gradient` | `angle_degrees`(0 = 顶部为白并向下渐隐,正值为顺时针)、`start` / `end`(权重由 1 降到 0 的轴向位置,0..1) | 渐变滤镜(天空、前景) |
| `radial_gradient` | `center_x` / `center_y`(0..1 相对位置)、`radius`(相对半对角线,0..1)、`feather`(0..1 的额外衰减带) | 暗角或主体聚光 |
| `luminosity_range` | `min` / `max`(0..255)、`feather`(区间之外的柔化过渡,亮度单位) | 高光、中间调或阴影 |
| `color_range` | `hue_center`(0..360)、`hue_width`(单侧宽度 1..180)、`feather`(额外角度) | 某一色相族(天空蓝、植被绿) |

   也可以用 `import_asset` 导入你自己的灰度图像。

2. 把返回的 `revision_id` 挂到操作上:

```json
{ "op": "curves", "master": [[0,0],[128,168],[255,255]],
  "mask": { "revision_id": "rev_...", "invert": false, "feather_px": 8.0 } }
```

   `invert`(默认 `false`)将权重翻转为 `1-w`;`feather_px`(默认 `0.0`)按当前图像
   坐标下的高斯 σ(像素)羽化蒙版边缘。

3. 给 `render_preview` 传入 `overlay:"mask"` 和 `mask_revision_id`,预览图会在权重
   超过 0.5 的区域染红、其余区域略微压暗,便于在正式应用前目视确认覆盖范围。

蒙版与 LUT 采用同一套引用机制,注意事项也相同:被引用的 id 计入 `recipe_hash`,
该配方只能在持有该蒙版的工作区内复现。

### 图层

配方可以携带 `layers` 图层栈,取代(或搭配)线性的 `operations` 列表。图层
自下而上合成,每个图层的 `ops` 先作用于自己的来源图像,再把结果混合到当前的
合成结果上:

```json
{
  "layers": [
    { "source": "base", "ops": [] },
    {
      "source": { "revision_id": "rev_..." },
      "ops": [{ "op": "blur", "sigma": 8 }],
      "blend_mode": "multiply",
      "opacity": 0.6
    }
  ],
  "operations": [
    { "op": "resize", "width": 1600 },
    { "op": "encode", "format": "webp", "quality": 82 }
  ]
}
```

- `source` 是 `"base"`(传给 `apply_transform` / `render_preview` 的输入
  修订版本)或 `{"revision_id": "rev_..."}`(工作区内的任意其他修订版本)。
  每个图层来源的尺寸都必须与 base 图像完全一致,否则会在任何像素处理开始之前
  返回结构化错误。
- `ops` 是普通的操作列表,只作用于该图层自己的来源。
- `mask`、`blend_mode`(默认 `"normal"`)与 `opacity`(默认 `1.0`)决定该图层
  如何混合到下方的图层上。
- 混合模式为 W3C 的 16 种模式之一:12 种 separable 模式(`normal` /
  `multiply` / `screen` / `overlay` / `darken` / `lighten` / `color_dodge` /
  `color_burn` / `hard_light` / `soft_light` / `difference` / `exclusion`)
  加上 4 种 non-separable 模式(`hue` / `saturation` / `color` /
  `luminosity`)。
- 存在 `layers` 时,顶层的 `operations` 就成为对合成结果的**收尾流程**——
  `resize` 与最终的 `encode` 都放在这里(`encode` 仍须最后出现且至多一次)。
- 调用 `explain_operation {"operation":"layers"}` 可获取完整参考。

## 预设

`apply_transform` 与 `render_preview` 接受 `recipe`(原始 DSL)或
`preset`(随包提供的具名配方,见 [`presets/`](presets))之一(二者互斥,且必须有其一):

| 预设 | 作用 |
|---|---|
| `eyecatch_16_9` | 居中裁剪为 16:9 → 宽 1600px → WebP q82 |
| `film_soft` | 柔和胶片感:平缓 S 形曲线 + 向亮度拉近 15% 的去饱和 |
| `product_clean` | 干净的电商产品风:接近中性的白平衡 + 色阶提亮 + 轻度锐化 |
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

## 名称由来

"atx" 是 **A**sset **T**ransform 的缩写;末尾的 `x` 沿用了 "transform" 的
惯用简写(如 xform / tx)。选它是因为便于作为简短易输入的二进制名和 crate
前缀(如 `atx-core`),与 PC 的 ATX 规格或 Markdown 的 ATX 风格标题并无关系。

## 许可证

MIT。详见 [LICENSE](LICENSE)。

如果 atx-mcp 帮你节省了时间,欢迎 [请我喝杯咖啡](https://buymeacoffee.com/gridhra) ☕
