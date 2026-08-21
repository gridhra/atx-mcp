//! 変換レシピ DSL(宣言的・直列パイプライン)と正規化・ハッシュ。
//!
//! 冪等性の要: 意味的に同一なレシピは `recipe_hash` が一致すること。
//! 正規化 JSON はキーをソートし、デフォルト値のフィールドも明示的に埋めて出力する。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 変換レシピ。operations を順に適用し、最後に(暗黙または明示の)encode で確定する。
///
/// v0.6 で **レイヤーグラフ**(`layers`)が加わった。`layers` を書くと:
///
/// 1. 各レイヤーが自分のソース画素(`base` = 入力画像 / `revision_id` = 別 revision)に
///    自分の `ops` を適用し、
/// 2. 先頭レイヤー(= backdrop)のキャンバスへ上から順に合成され、
/// 3. 最後に **トップレベルの `operations` が合成結果に対する仕上げパス**として走る。
///
/// `layers` を書かない v1 レシピは正規化 JSON がバイト単位で従来と一致し、
/// `recipe_hash` も不変(`skip_serializing_if = "Option::is_none"`)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TransformRecipe {
    pub operations: Vec<Operation>,
    /// レイヤーグラフ(v0.6)。省略時は従来どおり単一パイプライン。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layers: Option<Vec<Layer>>,
}

/// パイプラインの1オペレーション。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    /// EXIF Orientation タグに基づく正規化(回転・反転)。常に安全に適用可能。
    AutoOrient,
    /// 任意角度回転(正 = 時計回り、度数法)。
    Rotate {
        angle_degrees: f64,
        #[serde(default)]
        crop: RotateCrop,
    },
    /// クロップ / パディング。aspect_ratio("16:9")または rect のどちらか一方を指定。
    Crop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aspect_ratio: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rect: Option<Rect>,
        #[serde(default)]
        anchor: Anchor,
        #[serde(default)]
        mode: CropMode,
        /// mode=pad のときの余白色(CSS hex, 例 "#ffffff")。省略時は白。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pad_color: Option<String>,
        /// `rect` をどの座標系で解釈するか。`rect` と併用するときのみ有効
        /// (`aspect_ratio` と併用すると validate エラー)。
        ///
        /// - `current`(既定): これまでの op を適用した**現在の**画像の座標系。
        ///   従来どおりの挙動で、正規化 JSON にはこのフィールドが現れない
        ///   (= 既存レシピの `recipe_hash` はバイト単位で不変)。
        /// - `source`: **入力画像(EXIF orientation 正規化前)** の座標系。
        ///   エンジンが幾何 op(orientation 正規化 / rotate / crop / pad / resize)を
        ///   畳み込んだアフィン変換で矩形を写してからクロップする。
        ///
        /// # source 指定時の丸めとクランプ
        ///
        /// 矩形の 4 隅を写し、その**軸並行外接矩形**を取る。したがって
        /// **回転を挟んだ後は「見た目上傾いた四角形」ではなく、その外接矩形**
        /// (元矩形よりわずかに大きい領域)が切り出される。回転角が小さいほど差も小さい。
        /// 端の座標は half-away-from-zero で丸め、現在の画像範囲へクランプする。
        /// クランプが起きた場合は `EncodedOutput::warnings` に記録し、
        /// 交差が空になった場合は写像後の座標を含む構造化エラーを返す。
        #[serde(default, skip_serializing_if = "CoordinateSpace::is_current")]
        coordinate_space: CoordinateSpace,
    },
    /// リサイズ。width/height の少なくとも一方を指定。
    Resize {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
        #[serde(default)]
        fit: Fit,
        #[serde(default = "default_true")]
        without_enlargement: bool,
    },
    /// 色調整。各値は -1.0..=1.0(0 = 変更なし)。sharpness は 0.0..=1.0。
    Adjust {
        #[serde(default)]
        brightness: f64,
        #[serde(default)]
        contrast: f64,
        #[serde(default)]
        saturation: f64,
        #[serde(default)]
        sharpness: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<MaskRef>,
    },
    /// 遠近(台形/キーストーン)補正。`quad` または `vertical_degrees`/`horizontal_degrees`
    /// のどちらか一方の形式で指定する(排他)。射影変換のため、これ以降
    /// `coordinate_space: "source"` の crop は射影行列経由で写像される。
    Perspective {
        /// 入力画像内の四角形(tl, tr, br, bl の順、ピクセル座標)。
        /// この四角形が出力の長方形になるよう補正する。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quad: Option<[[f64; 2]; 4]>,
        /// 縦キーストーン角(度)。正 = 上辺が奥(上すぼまりを補正)。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vertical_degrees: Option<f64>,
        /// 横キーストーン角(度)。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        horizontal_degrees: Option<f64>,
        /// 余白色(CSS hex)。省略時は白。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pad_color: Option<String>,
    },
    /// 4×5 カラー行列(行優先、R'G'B'A' = M·[R G B A 1])。0..1 正規化値に対して適用し、
    /// 結果はクランプ。セピア・白黒・色相回転・チャンネルミキサーのメタ op。
    ColorMatrix {
        /// 長さ 20(4行 × 5列)。
        matrix: Vec<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<MaskRef>,
    },
    /// チャンネル別トーンカーブ。制御点 [x, y](0-255)列を単調3次補間し 256 LUT 化。
    /// master は RGB 共通(適用順: master → 各チャンネル)。
    Curves {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        master: Option<Vec<[u8; 2]>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        red: Option<Vec<[u8; 2]>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        green: Option<Vec<[u8; 2]>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blue: Option<Vec<[u8; 2]>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<MaskRef>,
    },
    /// レベル補正(master のみ)。内部的には curves 相当の 256 LUT に落ちる。
    Levels {
        #[serde(default)]
        in_black: u8,
        #[serde(default = "default_255")]
        in_white: u8,
        /// 0.1..=10.0。1.0 = 変更なし。
        #[serde(default = "default_gamma")]
        gamma: f64,
        #[serde(default)]
        out_black: u8,
        #[serde(default = "default_255")]
        out_white: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<MaskRef>,
    },
    /// ガウスぼかし。sigma は 0.1..=100.0。
    Blur {
        sigma: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<MaskRef>,
    },
    /// メディアンフィルタ。radius は 1..=16。
    Median {
        radius: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<MaskRef>,
    },
    /// アンシャープマスク。amount 0.0..=4.0、radius(gaussian σ)0.1..=50.0、
    /// threshold は輝度差がこの値以下の画素を保護。
    UnsharpMask {
        amount: f64,
        radius: f64,
        #[serde(default)]
        threshold: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<MaskRef>,
    },
    /// 3D LUT(.cube)適用。LUT はワークスペースへ import 済みの revision を参照する。
    /// revision は不変なので、参照 id をハッシュに含めるだけで決定論が保たれる。
    /// 注意: レシピの再現はワークスペース内でのみ保証される(他環境へは LUT アセットごと移す)。
    Lut {
        /// .cube アセットの revision id("rev_...")。
        lut_revision_id: String,
        /// 適用強度 0.0..=1.0(元画像との線形ブレンド)。既定 1.0。
        #[serde(default = "default_one")]
        strength: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<MaskRef>,
    },
    /// ホワイトバランス。temperature: 正=暖色へ / 負=寒色へ(-100..=100)、
    /// tint: 正=マゼンタへ / 負=グリーンへ(-100..=100)。0 = 変更なし。
    WhiteBalance {
        #[serde(default)]
        temperature: f64,
        #[serde(default)]
        tint: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<MaskRef>,
    },
    /// 色相域別 HSL 調整(Lightroom HSL パネル相当)。8 色相域それぞれに
    /// hue(-100..=100、隣接色相方向へのシフト)/ saturation / luminance(-100..=100)。
    /// 未指定の域は変更なし。域境界は滑らかに減衰(フェザリング)する。
    Hsl {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        red: Option<HslShift>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orange: Option<HslShift>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        yellow: Option<HslShift>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        green: Option<HslShift>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aqua: Option<HslShift>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blue: Option<HslShift>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        purple: Option<HslShift>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        magenta: Option<HslShift>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<MaskRef>,
    },
    /// 任意カーネル畳み込み。kernel は size×size(行優先)、size は 3/5/7/9。
    /// 出力 = (Σ kernel_i * px_i) / divisor + offset。端はクランプ。RGB のみ(A は不変)。
    Convolve {
        kernel: Vec<f64>,
        size: u32,
        #[serde(default = "default_one")]
        divisor: f64,
        #[serde(default)]
        offset: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<MaskRef>,
    },
    /// 出力エンコード指定。レシピ内で最後に1回のみ許可。省略時は入力フォーマット維持。
    Encode {
        format: OutputFormat,
        /// 1..=100。lossless フォーマット(png)では無視。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quality: Option<u8>,
        /// 出力のチャンネルビット深度。`8`(既定)または `16`。
        ///
        /// `16` は **png 出力でのみ**有効(他フォーマットは validate エラー)。
        /// 内部パイプラインは v0.4 以降 f32 リニアライトなので、16bit 出力では
        /// 8bit へ丸めずに階調をそのまま書き出せる(グラデーションのバンディング回避)。
        ///
        /// serde は `#[serde(default, skip_serializing_if = "Option::is_none")]`。
        /// 既定(未指定)のときは正規化 JSON にフィールドが現れないので、
        /// `bit_depth` を書かない既存レシピの canonical JSON はバイト単位で従来と一致し、
        /// **`recipe_hash` は不変**(v0.3 の `coordinate_space` と同じ手口)。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bit_depth: Option<u8>,
    },
    /// メタデータ剥離。
    StripMetadata {
        /// "all"(EXIF + ICC すべて破棄)| "gps"(v1 では all と同挙動)|
        /// "exif"(EXIF/GPS を確実に落とし、ICC は温存)
        #[serde(default)]
        scope: StripScope,
    },
}

/// 局所適用マスクの参照(v0.5)。
///
/// マスクはワークスペースへ import 済みの **画像 revision**(任意フォーマット)を参照する。
/// revision は不変なので、参照 id をハッシュに含めるだけで決定論が保たれる
/// (`lut` の参照と同じ設計。DESIGN.md §9.4 / §9.6)。
///
/// 重みは「マスク画像の **sRGB 符号値上の BT.709 輝度**」で、白 = 1.0(op を全量適用)、
/// 黒 = 0.0(op を適用しない)。マスクは**光ではなく被覆率**なので、線形光へ
/// 戻さず符号値のまま輝度を取る(§9.6)。マスクのアルファは無視する。
///
/// マスク画像の寸法が現在の画像と違う場合は、双線形補間で現在の寸法へ合わせる。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaskRef {
    /// マスク画像アセットの revision id("rev_...")。
    pub revision_id: String,
    /// true なら重みを反転する(w = 1 - w)。リサイズ後・フェザ前に適用。
    #[serde(default)]
    pub invert: bool,
    /// 境界のフェザ量。0 = なし。0.0..=200.0(ガウスぼかしの σ [px]、現在の画像座標)。
    #[serde(default)]
    pub feather_px: f64,
}

/// レイヤーグラフの 1 レイヤー(v0.6)。
///
/// ソース画素へ `ops` を適用し、その結果を(`mask` の重み × `opacity` × 画素アルファ)を
/// ソースアルファとして `blend_mode` で下のキャンバスへ合成する。
///
/// - 先頭レイヤー(index 0)は **backdrop** で、`blend_mode: normal` /
///   `opacity: 1.0` / `mask` 無しでなければならない(合成相手がまだ無いため)。
/// - `ops` に `encode` / `strip_metadata` は書けない(仕上げパス専用の op)。
/// - 合成後のレイヤー寸法は backdrop と一致していなければならない
///   (合わせるための resize / crop は `ops` に書く)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    /// このレイヤーの画素ソース。
    pub source: LayerSource,
    /// 合成前にこのレイヤーへ適用する op 列(通常のパイプラインと同じ語彙。
    /// `encode` / `strip_metadata` を除く)。
    #[serde(default)]
    pub ops: Vec<Operation>,
    /// 合成マスク。重みは「このレイヤーをどれだけ乗せるか」で、backdrop の寸法で解決する。
    /// 重み平面の作り方は op マスク(§9.6)と同一。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<MaskRef>,
    /// separable ブレンドモード(W3C compositing-1)。既定 `normal`。
    #[serde(default)]
    pub blend_mode: BlendMode,
    /// レイヤー不透明度 0.0..=1.0。既定 1.0。
    #[serde(default = "default_one")]
    pub opacity: f64,
}

/// レイヤーの画素ソース。
///
/// # serde 表現(atx-mcp との契約)
///
/// ```jsonc
/// {"source": "base"}                          // 入力画像そのもの
/// {"source": {"revision_id": "rev_abc123"}}   // ワークスペースの別 revision
/// ```
///
/// **untagged** を選んだ理由:
/// - 2 つの表現が JSON の**型レベルで排他**(string か object か)なので曖昧さが無い
/// - `{"source": {"kind": "base"}}` のようなラッパを増やさずに済み、
///   エージェントが書く JSON が短い(トークン規律)
/// - JSON Schema は `anyOf: [{enum: ["base"]}, {object with revision_id}]` に落ち、
///   スキーマ生成でも表現できる
/// - 正規化 JSON は「文字列」か「キー 1 個のオブジェクト」で、キー順の揺れが無い
///   = canonical 安定
///
/// `base` は untagged のユニットバリアントが serde では `null` としか往復しないため、
/// **1 バリアントだけの文字列 enum** [`BaseKeyword`] で表現している(表現は `"base"`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum LayerSource {
    /// 入力画像(EXIF orientation 正規化後)。JSON では `"base"`。
    Base(BaseKeyword),
    /// 別アセットの revision。JSON では `{"revision_id": "rev_..."}`。
    Revision {
        /// 画像アセットの revision id("rev_...")。
        revision_id: String,
    },
}

impl LayerSource {
    /// `{"source": "base"}` を作る。
    pub fn base() -> Self {
        LayerSource::Base(BaseKeyword::Base)
    }

    /// 入力画像を指すか。
    pub fn is_base(&self) -> bool {
        matches!(self, LayerSource::Base(_))
    }

    /// revision 参照ならその id。
    pub fn revision_id(&self) -> Option<&str> {
        match self {
            LayerSource::Revision { revision_id } => Some(revision_id),
            LayerSource::Base(_) => None,
        }
    }
}

/// `LayerSource::Base` の JSON 表現(文字列 `"base"`)を担う 1 バリアント enum。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BaseKeyword {
    #[default]
    Base,
}

/// separable ブレンドモード 12 種(W3C Compositing and Blending Level 1)。
///
/// 非 separable 系(hue / saturation / color / luminosity)は v0.7。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BlendMode {
    /// `B(Cb, Cs) = Cs`
    #[default]
    Normal,
    /// `B = Cb × Cs`
    Multiply,
    /// `B = Cb + Cs − Cb × Cs`
    Screen,
    /// `B = HardLight(Cs, Cb)`(引数を入れ替えた hard_light)
    Overlay,
    /// `B = min(Cb, Cs)`
    Darken,
    /// `B = max(Cb, Cs)`
    Lighten,
    /// `B = Cb == 0 ? 0 : Cs == 1 ? 1 : min(1, Cb / (1 − Cs))`
    ColorDodge,
    /// `B = Cb == 1 ? 1 : Cs == 0 ? 0 : 1 − min(1, (1 − Cb) / Cs)`
    ColorBurn,
    /// `B = Cs <= 0.5 ? Multiply(Cb, 2×Cs) : Screen(Cb, 2×Cs − 1)`
    HardLight,
    /// W3C の D(Cb) 区分(Cb <= 0.25 で多項式、それ以外は sqrt)を使う式
    SoftLight,
    /// `B = |Cb − Cs|`
    Difference,
    /// `B = Cb + Cs − 2 × Cb × Cs`
    Exclusion,
}

/// 色相域ごとの HSL シフト量。各値 -100..=100(0 = 変更なし)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HslShift {
    #[serde(default)]
    pub hue: f64,
    #[serde(default)]
    pub saturation: f64,
    #[serde(default)]
    pub luminance: f64,
}

impl Operation {
    /// この op に付いた局所適用マスク(あれば)。
    ///
    /// マスクを持てるのは**調整系 11 op のみ**(adjust / color_matrix / curves / levels /
    /// hsl / lut / white_balance / blur / median / unsharp_mask / convolve)。
    /// 幾何 op(resize / rotate / crop / perspective)は「一部だけリサイズ」に
    /// 意味が無いため対象外、encode / strip_metadata / auto_orient も同様。
    pub fn mask(&self) -> Option<&MaskRef> {
        match self {
            Operation::Adjust { mask, .. }
            | Operation::ColorMatrix { mask, .. }
            | Operation::Curves { mask, .. }
            | Operation::Levels { mask, .. }
            | Operation::Hsl { mask, .. }
            | Operation::Lut { mask, .. }
            | Operation::WhiteBalance { mask, .. }
            | Operation::Blur { mask, .. }
            | Operation::Median { mask, .. }
            | Operation::UnsharpMask { mask, .. }
            | Operation::Convolve { mask, .. } => mask.as_ref(),
            Operation::AutoOrient
            | Operation::Rotate { .. }
            | Operation::Crop { .. }
            | Operation::Resize { .. }
            | Operation::Perspective { .. }
            | Operation::Encode { .. }
            | Operation::StripMetadata { .. } => None,
        }
    }
}

fn default_one() -> f64 {
    1.0
}

fn default_255() -> u8 {
    255
}

fn default_gamma() -> f64 {
    1.0
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RotateCrop {
    /// 回転後の最大内接矩形でクロップ(余白なし)。
    #[default]
    LargestInscribedRect,
    /// 回転後の全体を含むキャンバス(余白は pad 色)。
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    #[default]
    Center,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CropMode {
    /// はみ出し分を切り落として比率を合わせる。
    #[default]
    Crop,
    /// 余白を足して比率を合わせる。
    Pad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    /// 指定ボックスを覆うようにリサイズ後、はみ出しをクロップ。
    #[default]
    Cover,
    /// 指定ボックスに収まるようにリサイズ(比率維持、クロップなし)。
    Contain,
    /// 比率を無視して指定サイズへ。
    Fill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    Jpeg,
    Png,
    Webp,
    Avif,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StripScope {
    /// EXIF も ICC も含め、あらゆるメタデータを落とす。
    #[default]
    All,
    /// GPS のみを狙った指定。v1 では EXIF ブロックごと落ちる(上位集合)。
    Gps,
    /// EXIF(GPS 含む)を確実に落とし、**ICC プロファイルは温存する**。
    ///
    /// Web 配信で「位置情報は消したいが色は動かしたくない」ケース向け。
    /// ICC の埋め込みに対応しているのは v1 では JPEG 出力のみで、
    /// PNG / WebP / AVIF 出力では従来どおり警告付きで破棄される。
    Exif,
}

/// `Crop { rect }` の座標系。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateSpace {
    /// これまでの op を適用した現在の画像の座標系(既定)。
    #[default]
    Current,
    /// 入力画像(EXIF orientation 正規化前)の座標系。
    Source,
}

impl CoordinateSpace {
    /// 既定値(= 正規化 JSON に出さない)かどうか。
    ///
    /// `skip_serializing_if` に渡すことで、`coordinate_space` を書かない既存レシピの
    /// 正規化 JSON がバイト単位で従来と一致し、`recipe_hash` が不変になる。
    pub fn is_current(&self) -> bool {
        matches!(self, CoordinateSpace::Current)
    }
}

/// レシピを正規化 JSON 文字列にする。
/// キーは辞書順、デフォルト値も明示的に serialize し、数値は serde_json の標準表現を用いる。
///
/// 「意味的に同一」の定義:
/// - フィールドの記述順は無関係(全階層でキーを辞書順に並べ替える)
/// - 省略されたフィールドと、デフォルト値を明示したフィールドは同一
///   (serde の `#[serde(default)]` でデシリアライズ時に埋まるため、
///   シリアライズ結果が一致する)
/// - `Option` フィールドの `null` 明示と省略も同一(どちらも `None` になる)
/// - 空白・インデントは正規化 JSON には含まれない(compact 表現)
/// - **f64 は 1e-6 グリッドに量子化してから出力する**。レシピの浮動小数
///   フィールド(rotate の angle_degrees、adjust の各値)は 1e-6 の意味的精度を
///   持つものと定義し、それより細かい差(JSON テキスト往復で生じる 1 ULP の
///   ずれを含む)は正規化で吸収する。整数(および数学的に整数の f64)の
///   表現は従来どおり変わらない。
pub fn canonical_json(recipe: &TransformRecipe) -> crate::Result<String> {
    let value = serde_json::to_value(recipe)
        .map_err(|e| crate::AtxError::InvalidRecipe(format!("serialization failed: {e}")))?;
    let mut out = String::new();
    write_canonical(&value, &mut out);
    Ok(out)
}

/// `serde_json::Value` を、全階層でキーを辞書順にした compact JSON として書き出す。
fn write_canonical(value: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            // serde_json の Map は既定で BTreeMap(辞書順)だが、
            // preserve_order feature の有無に依存しないよう明示的に並べ替える。
            let sorted: std::collections::BTreeMap<&String, &Value> = map.iter().collect();
            out.push('{');
            for (i, (k, v)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*k).clone()).to_string());
                out.push(':');
                write_canonical(v, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(v, out);
            }
            out.push(']');
        }
        // 非整数の f64 は 1e-6 グリッドへ量子化してから書き出す。
        // serde_json のテキストパーサは最短往復可能表現から 1 ULP ずれた値を
        // 返すことがあるため、量子化しないと JSON 往復でハッシュが変わりうる。
        Value::Number(n) if n.is_f64() => {
            let raw = n.as_f64().unwrap_or(f64::NAN);
            match serde_json::Number::from_f64(quantize(raw)) {
                Some(q) => out.push_str(&q.to_string()),
                // 非有限値など Number にできないものは元の表現のままにする。
                None => out.push_str(&n.to_string()),
            }
        }
        // その他のスカラ(整数・真偽値・文字列・null)は標準表現をそのまま使う。
        other => out.push_str(&other.to_string()),
    }
}

/// f64 を 1e-6 グリッドに量子化する(正規化 JSON 用)。
///
/// `|a - b| < 0.5e-6` の 2 値は同一の double に落ちるため、JSON テキスト往復で
/// 生じる 1 ULP のずれが正規化表現に漏れなくなる。非有限値や 2^53 を超えて
/// 丸めが無意味になる大きさの値はそのまま返す(recipe の値域は validate が
/// ±360 に制限しているため実運用では発生しない)。
fn quantize(v: f64) -> f64 {
    const MAX_EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
    if !v.is_finite() {
        return v;
    }
    let scaled = v * 1e6;
    if !scaled.is_finite() || scaled.abs() >= MAX_EXACT {
        return v;
    }
    scaled.round() / 1e6
}

/// 正規化 JSON の sha256(hex 小文字)。冪等性キーの片翼。
pub fn recipe_hash(recipe: &TransformRecipe) -> crate::Result<String> {
    use sha2::{Digest, Sha256};
    let canonical = canonical_json(recipe)?;
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    Ok(hex::encode(hasher.finalize()))
}

/// レシピの静的検証(encode の位置・重複、rect/aspect_ratio の排他、値域)。
///
/// ここで検証するのは入力バイト列に依存しない静的制約のみ。
/// rect が画像範囲に収まるか等の動的検証は `engine::apply_recipe` 側で行う。
pub fn validate(recipe: &TransformRecipe) -> crate::Result<()> {
    use crate::AtxError::InvalidRecipe;

    // トップレベルの `operations` は合成結果に対する**仕上げパス**。
    // layers がある場合に限り空を許す(合成しただけで出したいケース)。
    if recipe.operations.is_empty() && recipe.layers.is_none() {
        return Err(InvalidRecipe("operations must not be empty".into()));
    }
    validate_operations(&recipe.operations)?;

    if let Some(layers) = &recipe.layers {
        validate_layers(layers)?;
    }
    Ok(())
}

/// レイヤー列の静的検証(v0.6)。
fn validate_layers(layers: &[Layer]) -> crate::Result<()> {
    use crate::AtxError::InvalidRecipe;

    if layers.is_empty() {
        return Err(InvalidRecipe(
            "layers must not be empty when present (omit the key entirely for a \
             single-pipeline recipe)"
                .into(),
        ));
    }

    for (i, layer) in layers.iter().enumerate() {
        // ソース参照。
        if let Some(id) = layer.source.revision_id() {
            if id.is_empty() {
                return Err(InvalidRecipe(format!(
                    "layers[{i}] (source): revision_id must not be empty"
                )));
            }
            if !id.starts_with("rev_") {
                return Err(InvalidRecipe(format!(
                    "layers[{i}] (source): revision_id must start with \"rev_\", got {id:?}"
                )));
            }
        }

        // 不透明度。
        if !layer.opacity.is_finite() || !(0.0..=1.0).contains(&layer.opacity) {
            return Err(InvalidRecipe(format!(
                "layers[{i}]: opacity must be within 0.0..=1.0, got {}",
                layer.opacity
            )));
        }

        // 合成マスク(op マスクと同じ静的制約)。
        if let Some(mask) = &layer.mask {
            crate::ops::mask::validate(i, mask).map_err(|e| prefix_layer(i, e))?;
        }

        // 先頭レイヤーは backdrop: 合成相手がまだ無いので合成パラメータを持てない。
        if i == 0 {
            if layer.blend_mode != BlendMode::Normal {
                return Err(InvalidRecipe(format!(
                    "layers[0] is the backdrop (there is nothing underneath it to blend with), \
                     so blend_mode must be \"normal\", got {:?}",
                    layer.blend_mode
                )));
            }
            if layer.opacity != 1.0 {
                return Err(InvalidRecipe(format!(
                    "layers[0] is the backdrop (there is nothing underneath it to blend with), \
                     so opacity must be 1.0, got {}",
                    layer.opacity
                )));
            }
            if layer.mask.is_some() {
                return Err(InvalidRecipe(
                    "layers[0] is the backdrop (there is nothing underneath it to blend with), \
                     so it must not carry a mask; put the mask on a layer above it"
                        .into(),
                ));
            }
        }

        // レイヤー ops は仕上げパス専用 op を含めない。
        for (j, op) in layer.ops.iter().enumerate() {
            match op {
                Operation::Encode { .. } => {
                    return Err(InvalidRecipe(format!(
                        "layers[{i}].ops[{j}] (encode): encode is a finishing-pass-only operation; \
                         put it in the top-level operations, which run on the composite"
                    )));
                }
                Operation::StripMetadata { .. } => {
                    return Err(InvalidRecipe(format!(
                        "layers[{i}].ops[{j}] (strip_metadata): strip_metadata is a \
                         finishing-pass-only operation; put it in the top-level operations, \
                         which run on the composite"
                    )));
                }
                _ => {}
            }
        }
        validate_operations(&layer.ops).map_err(|e| prefix_layer(i, e))?;
    }
    Ok(())
}

/// レイヤー文脈で出たエラーメッセージにレイヤー番号を前置する。
///
/// 個々の op バリデータは `operations[j] (op): ...` という書式を共有しているので、
/// ここで包むだけで「どのレイヤーの何番目の op か」が両方名指しされる。
fn prefix_layer(i: usize, e: crate::AtxError) -> crate::AtxError {
    match e {
        crate::AtxError::InvalidRecipe(m) => {
            crate::AtxError::InvalidRecipe(format!("layers[{i}].ops: {m}"))
        }
        other => other,
    }
}

/// op 列の静的検証(トップレベルの `operations` とレイヤーの `ops` で共有)。
fn validate_operations(operations: &[Operation]) -> crate::Result<()> {
    use crate::AtxError::InvalidRecipe;

    if operations.is_empty() {
        return Ok(());
    }
    let last_index = operations.len() - 1;

    for (index, op) in operations.iter().enumerate() {
        // 局所適用マスクは op 種別に依らず同じ静的制約なので、まとめて検証する。
        if let Some(mask) = op.mask() {
            crate::ops::mask::validate(index, mask)?;
        }
        match op {
            Operation::AutoOrient | Operation::StripMetadata { .. } => {}
            // v0.2 で追加された op の静的検証は各実装モジュールに委譲する。
            Operation::Perspective {
                quad,
                vertical_degrees,
                horizontal_degrees,
                pad_color,
            } => crate::ops::perspective::validate(
                index,
                quad,
                vertical_degrees,
                horizontal_degrees,
                pad_color,
            )?,
            Operation::ColorMatrix { matrix, .. } => {
                crate::ops::color::validate_matrix(index, matrix)?
            }
            Operation::Curves {
                master,
                red,
                green,
                blue,
                ..
            } => crate::ops::color::validate_curves(index, master, red, green, blue)?,
            Operation::Levels {
                in_black,
                in_white,
                gamma,
                out_black,
                out_white,
                ..
            } => crate::ops::color::validate_levels(
                index, *in_black, *in_white, *gamma, *out_black, *out_white,
            )?,
            Operation::Lut {
                lut_revision_id,
                strength,
                ..
            } => crate::ops::lut::validate(index, lut_revision_id, *strength)?,
            Operation::WhiteBalance {
                temperature, tint, ..
            } => crate::ops::wb::validate(index, *temperature, *tint)?,
            Operation::Hsl {
                red,
                orange,
                yellow,
                green,
                aqua,
                blue,
                purple,
                magenta,
                ..
            } => crate::ops::hsl::validate(
                index,
                &[red, orange, yellow, green, aqua, blue, purple, magenta],
            )?,
            Operation::Convolve {
                kernel,
                size,
                divisor,
                offset,
                ..
            } => crate::ops::convolve::validate(index, kernel, *size, *divisor, *offset)?,
            Operation::Blur { sigma, .. } => crate::ops::blur::validate_blur(index, *sigma)?,
            Operation::Median { radius, .. } => crate::ops::blur::validate_median(index, *radius)?,
            Operation::UnsharpMask {
                amount,
                radius,
                threshold,
                ..
            } => crate::ops::blur::validate_unsharp(index, *amount, *radius, *threshold)?,
            Operation::Rotate { angle_degrees, .. } => {
                if !angle_degrees.is_finite() || !(-360.0..=360.0).contains(angle_degrees) {
                    return Err(InvalidRecipe(format!(
                        "operations[{index}] (rotate): angle_degrees must be within -360..=360, got {angle_degrees}"
                    )));
                }
            }
            Operation::Crop {
                aspect_ratio,
                rect,
                coordinate_space,
                ..
            } => {
                if *coordinate_space == CoordinateSpace::Source && rect.is_none() {
                    return Err(InvalidRecipe(format!(
                        "operations[{index}] (crop): coordinate_space \"source\" is only valid \
                         together with rect (aspect_ratio has no coordinate system to map)"
                    )));
                }
                match (aspect_ratio, rect) {
                    (Some(_), Some(_)) => {
                        return Err(InvalidRecipe(format!(
                        "operations[{index}] (crop): specify exactly one of aspect_ratio or rect, not both"
                    )));
                    }
                    (None, None) => {
                        return Err(InvalidRecipe(format!(
                            "operations[{index}] (crop): one of aspect_ratio or rect is required"
                        )));
                    }
                    (Some(ratio), None) => {
                        parse_aspect_ratio(ratio).ok_or_else(|| {
                        InvalidRecipe(format!(
                            "operations[{index}] (crop): aspect_ratio must be \"W:H\" with positive integers, got {ratio:?}"
                        ))
                    })?;
                    }
                    (None, Some(r)) => {
                        if r.width == 0 || r.height == 0 {
                            return Err(InvalidRecipe(format!(
                                "operations[{index}] (crop): rect width/height must be > 0"
                            )));
                        }
                    }
                }
            }
            Operation::Resize { width, height, .. } => {
                if width.is_none() && height.is_none() {
                    return Err(InvalidRecipe(format!(
                        "operations[{index}] (resize): at least one of width or height is required"
                    )));
                }
                if width == &Some(0) || height == &Some(0) {
                    return Err(InvalidRecipe(format!(
                        "operations[{index}] (resize): width/height must be > 0"
                    )));
                }
            }
            Operation::Adjust {
                brightness,
                contrast,
                saturation,
                sharpness,
                ..
            } => {
                for (name, v) in [
                    ("brightness", brightness),
                    ("contrast", contrast),
                    ("saturation", saturation),
                ] {
                    if !v.is_finite() || !(-1.0..=1.0).contains(v) {
                        return Err(InvalidRecipe(format!(
                            "operations[{index}] (adjust): {name} must be within -1.0..=1.0, got {v}"
                        )));
                    }
                }
                if !sharpness.is_finite() || !(0.0..=1.0).contains(sharpness) {
                    return Err(InvalidRecipe(format!(
                        "operations[{index}] (adjust): sharpness must be within 0.0..=1.0, got {sharpness}"
                    )));
                }
            }
            Operation::Encode {
                format,
                quality,
                bit_depth,
            } => {
                if index != last_index {
                    return Err(InvalidRecipe(format!(
                        "operations[{index}] (encode): encode must be the last operation"
                    )));
                }
                if let Some(q) = quality {
                    if !(1..=100).contains(q) {
                        return Err(InvalidRecipe(format!(
                            "operations[{index}] (encode): quality must be within 1..=100, got {q}"
                        )));
                    }
                }
                if let Some(depth) = bit_depth {
                    if !matches!(depth, 8 | 16) {
                        return Err(InvalidRecipe(format!(
                            "operations[{index}] (encode): bit_depth must be 8 or 16, got {depth}"
                        )));
                    }
                    if *depth == 16 && *format != OutputFormat::Png {
                        return Err(InvalidRecipe(format!(
                            "operations[{index}] (encode): bit_depth 16 is only supported for \
                             png output, got {format:?}"
                        )));
                    }
                }
            }
        }
    }

    let encode_count = operations
        .iter()
        .filter(|op| matches!(op, Operation::Encode { .. }))
        .count();
    if encode_count > 1 {
        return Err(InvalidRecipe(format!(
            "at most one encode operation is allowed, got {encode_count}"
        )));
    }

    Ok(())
}

/// "W:H" 形式のアスペクト比を (w, h) にパースする。両方正の整数のときのみ Some。
pub(crate) fn parse_aspect_ratio(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(':')?;
    let w: u32 = w.parse().ok()?;
    let h: u32 = h.parse().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

/// CSS hex カラー("#rgb" / "#rrggbb" / "#rrggbbaa")を RGBA8 にパースする。
pub(crate) fn parse_hex_color(s: &str) -> Option<[u8; 4]> {
    let s = s.strip_prefix('#')?;
    let nib = |c: u8| -> Option<u8> { (c as char).to_digit(16).map(|d| d as u8) };
    let b = s.as_bytes();
    match b.len() {
        3 | 4 => {
            let mut out = [255u8; 4];
            for (i, c) in b.iter().enumerate() {
                let v = nib(*c)?;
                out[i] = v * 17;
            }
            Some(out)
        }
        6 | 8 => {
            let mut out = [255u8; 4];
            for (i, pair) in b.chunks(2).enumerate() {
                out[i] = nib(pair[0])? * 16 + nib(pair[1])?;
            }
            Some(out)
        }
        _ => None,
    }
}
