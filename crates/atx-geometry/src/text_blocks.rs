//! 文字ブロックの検出(run-length smearing + 連結成分)。
//!
//! `detect_document`(用紙の四角形)が「どこを補正するか」を返すのに対し、
//! こちらは「どこに文字らしい塊があり、その文字が読める大きさかどうか」を返す。
//! 文字認識(OCR)はしない。文字らしい矩形と行の高さを測るだけで、
//! 実際に切り出すかどうかの判断はホスト AI に委ねる(read-only)。
//!
//! # パイプライン(DESIGN.md §9.15)
//!
//! 1. グレースケール化 + 長辺 ≤ `working_long_edge` へ Triangle 縮小
//!    ([`crate::downscale_gray`] を `detect_tilt` / `detect_document` と共用)
//! 2. Otsu(`imageproc::contrast::otsu_level`)で二値化。
//!    インク(文字)は**少数派の側**とする(黒画素が過半なら白黒を入れ替える)ので、
//!    白地に黒文字でも黒地に白文字でも同じに扱える
//! 3. 水平方向の run-length smearing: 各行で、インク画素に挟まれた白ギャップが
//!    `gap_px`(作業画像長辺の 1%)以下なら塗る。単語が 1 本の行に繋がる
//! 4. `imageproc::region_labelling::connected_components`(8 近傍)で成分に分け、
//!    成分ごとの外接矩形を 1 パスで求める
//! 5. 成分のうち「文字の行らしくない」ものを捨てる: 外接矩形の面積が作業画像面積の
//!    `min_block_area_ratio` 未満(ノイズ点)または 90% 超(紙面全体を囲った成分)、
//!    高さが作業画像高さの 10% 超(空・壁・影のような写真の領域)、
//!    縦が横より長い(横書きの 1 行は横に長い。**縦書きは対象外**)。
//!    残りを縦方向にマージする: x 範囲が重なり、縦ギャップが
//!    「行高の中央値の 1.5 倍」と「隣接ギャップの中央値の 1.5 倍」の**大きいほう**
//!    より小さい組を、変化が止まるまで繰り返し結合する → ブロック(段落)
//! 6. ブロックごとに、**smearing 前**のインク画素の水平投影プロファイル
//!    (行ごとのインク画素数 > 0 の連続区間)を取り、区間数を `line_count`、
//!    区間高さの中央値を `median_line_height_px`(原寸換算)とする
//!    極端に高い行(1 行の高さ上限超え)を含むブロック、
//!    行の高さの合計がブロック高さの 25% に届かないブロックは、
//!    文字ではなく縞模様(建物の窓・手すり)を拾ったものとして捨てる
//! 7. ブロックが 1 つも残らなかった場合、インクが多い(画素比 5% 以上・成分 200 個以上)なら
//!    「インクは多いが横書きの行は無い」事実を警告に足す([`dense_ink_warning`])。
//!    書字方向の推定はしない(実写で偽陽性を出したため撤回した。同関数の doc 参照)
//! 8. 読み順(上→下。y 範囲が重なるブロック同士は左→右)に並べ、`max_blocks` で切る。
//!    ただし帯分割(`recommended_bands`)が使う行位置は**切る前の全ブロック**から集める
//!    (切った後だけで作ると、ブロックが多いページで帯が途中で終わる)
//!
//! # 切り方は 3 通り(`legibility.strategy`)
//!
//! 「長辺 1568 のプレビューで行高 16px 以上」を満たす切り出しを返す。
//! どう切ったかは `strategy` で名指しする([`legibility_of`]):
//!
//! - `whole`: 画像全体でもう十分 → 切らない
//! - `bands`: 縦が律速 → 幅いっぱいの横帯(境界は行間、隣と 1 行重ねる)
//! - `blocks`: **横幅が律速** → 横帯では救えないので、ブロック単位の切り出し
//!   ([`block_crops`])。実測(4080x3072 のメニュー写真、行高 13px = 1568 換算 5.0px)
//!   では、横帯を 1 本にしても幅 4080 が縮小率を決めてしまい読めなかった。
//!   以前はこの場合に画像全体 1 本 + 警告だけを返しており、次の一手が無かった
//!
//! # 「縦ギャップの中央値」を併用する理由(DESIGN からの上乗せ)
//!
//! DESIGN は縦マージの閾値を「行高の中央値 × 1.5」と書いていたが、これだけでは
//! **行間が広い版面**(行送りが行高の 3 倍など)で 1 行ごとに別ブロックへ割れる。
//! 合成書類フィクスチャ(`tests/fixtures/synthetic_document.png`)がまさにそれで、
//! 行高 17px・行送り 51px(ギャップ 34px)なので 17 × 1.5 = 25.5px では届かない。
//! そこで「x 範囲が重なる上下の隣接ギャップ」の中央値も取り、その 1.5 倍と
//! 大きいほうを閾値にする。段落間の空き(同フィクスチャでは 85〜97px)は
//! 行間ギャップの中央値(34px)の 1.5 倍 = 51px を超えるので、段落の区切りは保たれる。
//!
//! # 決定論
//!
//! - 全段整数演算。`HashMap` の反復順に依存する処理は書かない
//! - f64 は最後の比率(`ink_ratio` / `text_like_area_ratio` /
//!   `line_height_at_1568_px`)だけで使い、[`crate::round3`] で 1e-3 に量子化する
//! - 並べ替えは「y 範囲の重なりで行にまとめてから x 昇順」の 2 段階で行う
//!   (重なり判定を比較関数に持ち込むと全順序にならないため)

use image::{DynamicImage, GrayImage, Luma};
use imageproc::contrast::otsu_level;
use imageproc::region_labelling::{connected_components, Connectivity};
use serde::Serialize;

use crate::{downscale_gray, round3};

/// ブロックが 1 つも取れなかったときの警告。
const REASON_NO_BLOCKS: &str = "no_text_like_regions";
/// `max_blocks` の上限。
const MAX_BLOCKS_LIMIT: usize = 128;
/// 外接矩形が作業画像面積のこの割合を超える成分は捨てる(紙面全体を囲った成分)。
const MAX_AREA_PERCENT: u64 = 90;
/// smearing の白ギャップ許容量: 作業画像長辺のこの割合(%)。
const SMEAR_GAP_PERCENT: u32 = 1;
/// 縦マージ閾値の係数(分子 / 分母 = 1.5)。
const MERGE_NUM: u32 = 3;
const MERGE_DEN: u32 = 2;
/// 縦マージの反復上限(通常は数回で収束する)。
const MERGE_MAX_PASSES: usize = 32;
/// 1 成分(= smearing 後の 1 行)の高さの上限: 作業画像高さのこの割合(%)。
/// これを超える塊は「文字の行」ではなく写真の領域(空・壁・影)と見なして捨てる。
const MAX_LINE_HEIGHT_PERCENT: u32 = 10;
/// 上の高さ上限の下限値(小さい画像で何も残らなくならないように)。
const MIN_LINE_HEIGHT_LIMIT: u32 = 8;
/// ブロック内の「行が占める高さの合計」がブロック高さのこの割合(%)を下回ったら
/// 文字ブロックとは見なさない(写真の中の縞模様・建物の窓のような偽陽性を落とす)。
const MIN_LINE_COVERAGE_PERCENT: u32 = 25;
/// 面積フィルタ通過後に扱う成分数の上限(病的な入力での計算量の上限)。
const MAX_COMPONENTS: usize = 2048;
/// 読みやすさを評価するプレビュー長辺(`render_preview` の実用値)。
const LEGIBLE_LONG_EDGE: u64 = 1568;
/// この行高(プレビュー上の px)を下回ると読み取りが不安定になる。
const MIN_LEGIBLE_LINE_HEIGHT: u64 = 16;
/// `recommended_bands` の本数上限。
const MAX_BANDS: usize = 16;
/// 「インクは多いのに行が取れない」警告の下限: インク画素が全体のこの %以上。
/// 文字で埋まった紙面は Otsu 後のインクが数〜20% を占める。5% 未満は
/// 「ロゴや見出しが少しある画像」で、行が取れなくても不思議はないので黙る。
const DENSE_INK_PERCENT: u64 = 5;
/// 同警告の下限: smearing 前の連結成分数。文字が数百字あれば紙面と呼べる。
/// 罫線数本・アイコン数個ではここに届かない。
const DENSE_INK_MIN_COMPONENTS: usize = 200;
/// 切り方の識別子(MCP が返す面なので英語)。
/// `whole` = 画像全体で読める、`bands` = 横帯、`blocks` = ブロック単位の切り出し。
const STRATEGY_WHOLE: &str = "whole";
const STRATEGY_BANDS: &str = "bands";
const STRATEGY_BLOCKS: &str = "blocks";

/// 検出パラメータ。
pub struct TextBlockParams {
    /// 返すブロック数の上限(1..=128)。
    pub max_blocks: usize,
    /// 作業画像面積に対する、1 成分の最小外接矩形面積比。これ未満はノイズとして捨てる。
    pub min_block_area_ratio: f64,
    /// 検出処理前に長辺をこのサイズまで縮小する。
    pub working_long_edge: u32,
}

impl Default for TextBlockParams {
    fn default() -> Self {
        Self {
            max_blocks: 32,
            min_block_area_ratio: 0.00005,
            working_long_edge: 1536,
        }
    }
}

/// 原寸(EXIF orientation 正規化後)の画素座標による矩形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct BlockRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// 文字らしい 1 ブロック(見出し・段落など)。
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct TextBlock {
    /// Bounding box in original pixel coordinates.
    pub rect: BlockRect,
    /// Number of text lines found inside the block.
    pub line_count: u32,
    /// Median height of those lines, in original pixels.
    pub median_line_height_px: u32,
    /// Fraction of the block's area covered by ink (0..=1).
    pub ink_ratio: f64,
}

/// そのまま recipe に貼れる `crop` op。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct SuggestedCrop {
    /// Always `"crop"`.
    pub op: String,
    /// Crop rectangle in original pixel coordinates.
    pub rect: BlockRect,
}

/// 読みやすさの見積もりと、読むための帯分割。
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Legibility {
    /// How `recommended_bands` was cut: `"whole"` (the text is already large
    /// enough, one crop covering the image), `"bands"` (full-width horizontal
    /// bands) or `"blocks"` (per-block crops, used when the image is too wide
    /// for full-width bands to help).
    pub strategy: String,
    /// Median line height after shrinking the whole image to long edge 1568,
    /// in preview pixels. Below ~16 the text is usually too small to read.
    pub line_height_at_1568_px: f64,
    /// Crops to read the image in, in reading order, each a `crop` operation
    /// ready to paste into a recipe before `render_preview` with
    /// `long_edge: 1568`. With `strategy: "bands"` they are full-width
    /// horizontal bands whose edges are aligned to gaps between text lines and
    /// which overlap by one line; they cover the vertical span that holds text,
    /// so a blank margin below the last line may be left out. With
    /// `strategy: "blocks"` each crop is one text block (or a few adjacent ones)
    /// padded by one line height, sized so the lines inside render at 16px or
    /// more at `long_edge: 1568`. A single crop covering the whole image
    /// (`strategy: "whole"`) means no splitting is needed. At most 16 crops are
    /// returned; when that is not enough, a warning names what they leave out.
    pub recommended_bands: Vec<SuggestedCrop>,
}

/// 文字ブロック検出の結果。MCP structuredContent 互換。
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct TextBlockDetection {
    /// Text-like blocks in reading order (top to bottom, then left to right).
    pub blocks: Vec<TextBlock>,
    /// Fraction of the image area covered by the returned blocks (0..=1).
    pub text_like_area_ratio: f64,
    /// Median line height across all detected lines, in original pixels.
    /// None when no block was found.
    pub median_line_height_px: Option<u32>,
    /// Legibility estimate. None when no block was found.
    pub legibility: Option<Legibility>,
    pub warnings: Vec<String>,
}

impl TextBlockDetection {
    /// 文字らしい領域が無かったときの結果。
    fn none() -> Self {
        Self {
            blocks: Vec::new(),
            text_like_area_ratio: 0.0,
            median_line_height_px: None,
            legibility: None,
            warnings: vec![REASON_NO_BLOCKS.to_string()],
        }
    }
}

/// 作業解像度での矩形(両端含む)。
#[derive(Debug, Clone, Copy)]
struct Rect {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl Rect {
    fn width(&self) -> u32 {
        self.x1 - self.x0 + 1
    }
    fn height(&self) -> u32 {
        self.y1 - self.y0 + 1
    }
    fn area(&self) -> u64 {
        self.width() as u64 * self.height() as u64
    }
    fn x_overlaps(&self, other: &Rect) -> bool {
        self.x0 <= other.x1 && other.x0 <= self.x1
    }
    fn y_overlaps(&self, other: &Rect) -> bool {
        self.y0 <= other.y1 && other.y0 <= self.y1
    }
    /// 縦方向の隙間(重なっていれば 0)。
    fn y_gap(&self, other: &Rect) -> u32 {
        if self.y_overlaps(other) {
            0
        } else if self.y1 < other.y0 {
            other.y0 - self.y1 - 1
        } else {
            self.y0 - other.y1 - 1
        }
    }
    fn union(&self, other: &Rect) -> Rect {
        Rect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }
}

/// 文字らしいブロックを検出する(read-only、決定論)。
///
/// 同一入力に対して常に同一の結果を返す。
pub fn detect_text_blocks(image: &DynamicImage, params: &TextBlockParams) -> TextBlockDetection {
    let (orig_w, orig_h) = (image.width(), image.height());
    if orig_w == 0 || orig_h == 0 {
        return TextBlockDetection::none();
    }
    let max_blocks = params.max_blocks.clamp(1, MAX_BLOCKS_LIMIT);

    let gray = downscale_gray(image, params.working_long_edge.clamp(64, 8192));
    let (w, h) = gray.dimensions();
    if w < 8 || h < 8 {
        return TextBlockDetection::none();
    }

    let ink = binarize(&gray);
    let gap_px = ((w.max(h) * SMEAR_GAP_PERCENT) / 100).max(1);
    let smeared = smear_horizontal(&ink, gap_px);
    // ブロックが 1 つも取れなかったときだけ、「インクは多いのに行が無い」事実を足す
    // (計数はその場合しか払わない)。
    let none_with_ink_note = || {
        let mut d = TextBlockDetection::none();
        if let Some(warning) = dense_ink_warning(&ink) {
            d.warnings.push(warning);
        }
        d
    };

    let work_area = w as u64 * h as u64;
    let min_px = ((work_area as f64) * params.min_block_area_ratio.clamp(0.0, 1.0)).ceil() as u64;
    let min_px = min_px.max(1);
    let max_px = work_area * MAX_AREA_PERCENT / 100;
    let max_line_h = (h * MAX_LINE_HEIGHT_PERCENT / 100).max(MIN_LINE_HEIGHT_LIMIT);
    let comps = components(&smeared, min_px, max_px, max_line_h);
    if comps.is_empty() {
        return none_with_ink_note();
    }

    let merged = merge_vertically(comps);

    // 読み順(y 範囲が重なるものを行にまとめ、行内は x 昇順)。
    let ordered = reading_order(merged);

    // ブロックごとの行構造(smearing 前のインクで測る)。
    let mut blocks: Vec<TextBlock> = Vec::new();
    let mut all_line_heights: Vec<u32> = Vec::new();
    let mut line_rows_work: Vec<(u32, u32)> = Vec::new();
    // `max_blocks` に達しても走査は止めない: 帯分割(`recommended_bands`)は
    // 「文字がどこまで続くか」を知る必要があるので、**切られたブロックの行も**数える。
    // 止めてしまうと、ブロックが多いページで帯が途中で終わっていた。
    for rect in ordered.iter() {
        let lines = line_runs(&ink, rect);
        if lines.is_empty() {
            continue;
        }
        // ブロックの行はどれも「1 行の高さの上限」以下でなければならない。
        // 写真の領域を跨いだブロックはここで 1 本の極端に高い行になって落ちる。
        if lines.iter().any(|&(a, b)| b - a + 1 > max_line_h) {
            continue;
        }
        // 行がブロックの高さをほとんど埋めていないなら、縞模様を拾っただけ。
        let covered_rows: u32 = lines.iter().map(|&(a, b)| b - a + 1).sum();
        if covered_rows * 100 < rect.height() * MIN_LINE_COVERAGE_PERCENT {
            continue;
        }
        // 行位置は全ブロック分(帯分割が「文字の続く範囲」を知るため)。
        line_rows_work.extend(lines.iter().copied());
        if blocks.len() >= max_blocks {
            continue;
        }
        let mut heights: Vec<u32> = lines.iter().map(|&(a, b)| b - a + 1).collect();
        let median_work = median(&mut heights);
        // 行の高さは**返すブロック**の分だけ集める(`median_line_height_px` は
        // 返した blocks を説明する数として据え置く)。
        all_line_heights.extend(heights.iter().copied());
        let ink_px = ink_count(&ink, rect);
        blocks.push(TextBlock {
            rect: to_original(rect, w, h, orig_w, orig_h),
            line_count: lines.len() as u32,
            median_line_height_px: scale_len(median_work, h, orig_h).max(1),
            ink_ratio: round3((ink_px as f64 / rect.area() as f64).clamp(0.0, 1.0)),
        });
    }
    if blocks.is_empty() {
        return none_with_ink_note();
    }

    let median_line_work = median(&mut all_line_heights);
    let median_line_orig = scale_len(median_line_work, h, orig_h).max(1);

    let covered: u64 = blocks
        .iter()
        .map(|b| b.rect.width as u64 * b.rect.height as u64)
        .sum();
    let orig_area = orig_w as u64 * orig_h as u64;
    let text_like_area_ratio = round3((covered as f64 / orig_area as f64).clamp(0.0, 1.0));

    // 帯分割のための行位置(原寸、重なりを潰して昇順)。
    let rows = normalized_rows(line_rows_work, h, orig_h);
    let mut warnings = Vec::new();
    let legibility = legibility_of(
        median_line_orig,
        orig_w,
        orig_h,
        &rows,
        &blocks,
        &mut warnings,
    );

    TextBlockDetection {
        blocks,
        text_like_area_ratio,
        median_line_height_px: Some(median_line_orig),
        legibility: Some(legibility),
        warnings,
    }
}

/// Otsu で二値化し、**少数派の側**をインク(255)にしたマスクを返す。
fn binarize(gray: &GrayImage) -> GrayImage {
    let level = otsu_level(gray);
    let mut dark: u64 = 0;
    for p in gray.pixels() {
        if p[0] <= level {
            dark += 1;
        }
    }
    let total = gray.width() as u64 * gray.height() as u64;
    let ink_is_dark = dark * 2 <= total;
    let mut out = GrayImage::new(gray.width(), gray.height());
    for (dst, src) in out.pixels_mut().zip(gray.pixels()) {
        let is_ink = if ink_is_dark {
            src[0] <= level
        } else {
            src[0] > level
        };
        *dst = Luma([if is_ink { 255 } else { 0 }]);
    }
    out
}

/// 「インクは多いのに横書きの行が 1 つも取れなかった」ときの警告(事実の報告のみ)。
///
/// 文字らしさフィルタは「横 > 縦」の成分しか通さない(横書きの 1 行は横に長い)ので、
/// 縦書きの面では 0 ブロック + `no_text_like_regions` になる。設計どおりだが、
/// 実写(朝日新聞 1879、1600x2281 の縦書き)のように**文字で埋まったページ**に
/// 「文字なし」だけを返すと、ホスト AI に次の一手が無い。
///
/// # なぜ「縦書きの推定」をやめたのか
///
/// 最初は縦方向にも smearing して「縦長成分 vs 横長成分」の比で縦書きを推定していたが、
/// **横書き**の露語新聞(3 段組、2240x3255)で偽陽性を出した
/// (縦 1357 : 横 289)。密な文字面では縦 smearing が行同士を縦に繋いでしまい、
/// 単語や文字の並びが縦長成分になる。書字方向の推定は実写に耐えないので撤回し、
/// **観測した事実だけ**を述べる警告に置き換えた: インクは多い / 横書きの行は無い。
/// 判断(回転して再実行するか、そのままプレビューで読むか)はホスト AI に委ねる。
///
/// 条件は「ブロックが 1 つも取れなかった」ことに加えて、
/// インク画素比 ≥ [`DENSE_INK_PERCENT`]% かつ smearing 前の連結成分数 ≥
/// [`DENSE_INK_MIN_COMPONENTS`]。ブロックが 1 つでも取れた画像では**出さない**
/// (偽陽性の余地を残さないため)。
///
/// 決定論: 画素の計数と `connected_components`(走査順は決定的)の個数比較だけ。
fn dense_ink_warning(ink: &GrayImage) -> Option<String> {
    let total = ink.width() as u64 * ink.height() as u64;
    if total == 0 {
        return None;
    }
    let ink_px = ink.pixels().filter(|p| p[0] != 0).count() as u64;
    if ink_px * 100 < total * DENSE_INK_PERCENT {
        return None;
    }
    if component_boxes(ink).len() < DENSE_INK_MIN_COMPONENTS {
        return None;
    }
    Some(
        "dense ink but no horizontal text lines were found; detect_text_blocks handles \
         horizontal writing only (vertical Japanese/Chinese text is not supported) - if the \
         text runs top-to-bottom, rotate 90 degrees with \
         {\"op\":\"rotate\",\"angle_degrees\":90} and re-run, or read the page directly with \
         render_preview long_edge 1568"
            .to_string(),
    )
}

/// 連結成分(8 近傍)の外接矩形を 1 パスで集める。
///
/// 決定論: `connected_components` のラベル付けと走査順は決定的。
fn component_boxes(mask: &GrayImage) -> Vec<Rect> {
    let labels = connected_components(mask, Connectivity::Eight, Luma([0u8]));
    let mut boxes: Vec<Option<Rect>> = Vec::new();
    for (x, y, p) in labels.enumerate_pixels() {
        let label = p[0] as usize;
        if label == 0 {
            continue;
        }
        if label >= boxes.len() {
            boxes.resize(label + 1, None);
        }
        match &mut boxes[label] {
            Some(r) => {
                r.x0 = r.x0.min(x);
                r.y0 = r.y0.min(y);
                r.x1 = r.x1.max(x);
                r.y1 = r.y1.max(y);
            }
            slot => {
                *slot = Some(Rect {
                    x0: x,
                    y0: y,
                    x1: x,
                    y1: y,
                })
            }
        }
    }
    boxes.into_iter().flatten().collect()
}

/// 水平方向の run-length smearing。インク画素に挟まれた `gap_px` 以下の
/// 白ギャップを塗り、単語を行に繋げる。
fn smear_horizontal(ink: &GrayImage, gap_px: u32) -> GrayImage {
    let (w, h) = ink.dimensions();
    let mut out = ink.clone();
    for y in 0..h {
        let mut last: Option<u32> = None;
        for x in 0..w {
            if ink.get_pixel(x, y)[0] == 0 {
                continue;
            }
            if let Some(lx) = last {
                if x > lx + 1 && x - lx - 1 <= gap_px {
                    for fx in (lx + 1)..x {
                        out.put_pixel(fx, y, Luma([255]));
                    }
                }
            }
            last = Some(x);
        }
    }
    out
}

/// 連結成分の外接矩形を 1 パスで求め、面積と行高のフィルタを掛ける。
///
/// 成分数が [`MAX_COMPONENTS`] を超えたら面積の大きい順に切る
/// (同面積は走査順 = ラベル順で安定)。
fn components(smeared: &GrayImage, min_px: u64, max_px: u64, max_line_h: u32) -> Vec<Rect> {
    let labels = connected_components(smeared, Connectivity::Eight, Luma([0u8]));
    let mut boxes: Vec<Option<Rect>> = Vec::new();
    for (x, y, p) in labels.enumerate_pixels() {
        let label = p[0] as usize;
        if label == 0 {
            continue;
        }
        if label >= boxes.len() {
            boxes.resize(label + 1, None);
        }
        match &mut boxes[label] {
            Some(r) => {
                r.x0 = r.x0.min(x);
                r.y0 = r.y0.min(y);
                r.x1 = r.x1.max(x);
                r.y1 = r.y1.max(y);
            }
            slot => {
                *slot = Some(Rect {
                    x0: x,
                    y0: y,
                    x1: x,
                    y1: y,
                })
            }
        }
    }
    let mut kept: Vec<Rect> = boxes
        .into_iter()
        .flatten()
        .filter(|r| {
            let a = r.area();
            // 横書きの 1 行は「縦より横に長い」。縦長の塊は文字の行ではない。
            a >= min_px && a <= max_px && r.height() <= max_line_h && r.width() > r.height()
        })
        .collect();
    if kept.len() > MAX_COMPONENTS {
        kept.sort_by_key(|r| std::cmp::Reverse(r.area()));
        kept.truncate(MAX_COMPONENTS);
    }
    kept.sort_by_key(|r| (r.y0, r.x0, r.y1, r.x1));
    kept
}

/// 縦方向のマージ(x 範囲が重なり、縦ギャップが閾値未満の組を収束まで結合)。
fn merge_vertically(comps: Vec<Rect>) -> Vec<Rect> {
    let threshold = merge_threshold(&comps);
    let mut rects = comps;
    for _ in 0..MERGE_MAX_PASSES {
        let mut merged: Vec<Rect> = Vec::with_capacity(rects.len());
        let mut changed = false;
        for r in rects.iter() {
            let mut cur = *r;
            let mut i = 0;
            while i < merged.len() {
                if merged[i].x_overlaps(&cur) && merged[i].y_gap(&cur) < threshold {
                    cur = cur.union(&merged[i]);
                    merged.remove(i);
                    changed = true;
                } else {
                    i += 1;
                }
            }
            merged.push(cur);
        }
        merged.sort_by_key(|r| (r.y0, r.x0, r.y1, r.x1));
        rects = merged;
        if !changed {
            break;
        }
    }
    rects
}

/// 縦マージの閾値(作業解像度の px)。
///
/// 「行高の中央値 × 1.5」と「x 範囲が重なる上下隣接ギャップの中央値 × 1.5」の
/// 大きいほう。後者はモジュール doc のとおり、行間が広い版面のための上乗せ。
fn merge_threshold(comps: &[Rect]) -> u32 {
    let mut heights: Vec<u32> = comps.iter().map(|r| r.height()).collect();
    let med_h = median(&mut heights);
    let mut gaps: Vec<u32> = Vec::new();
    for (i, a) in comps.iter().enumerate() {
        let mut best: Option<u32> = None;
        for (j, b) in comps.iter().enumerate() {
            if i == j || b.y0 <= a.y1 || !a.x_overlaps(b) {
                continue;
            }
            let g = a.y_gap(b);
            best = Some(best.map_or(g, |cur: u32| cur.min(g)));
        }
        if let Some(g) = best {
            gaps.push(g);
        }
    }
    let med_gap = if gaps.is_empty() {
        0
    } else {
        median(&mut gaps)
    };
    let from_h = med_h.saturating_mul(MERGE_NUM) / MERGE_DEN;
    let from_gap = med_gap.saturating_mul(MERGE_NUM) / MERGE_DEN;
    from_h.max(from_gap).max(2)
}

/// 読み順に並べる: y 範囲が重なるものを 1 行にまとめ、行内は x 昇順。
///
/// 比較関数に「重なり」を持ち込むと全順序にならないので 2 段階で行う。
fn reading_order(mut rects: Vec<Rect>) -> Vec<Rect> {
    rects.sort_by_key(|r| (r.y0, r.x0, r.y1, r.x1));
    let mut out: Vec<Rect> = Vec::with_capacity(rects.len());
    let mut row: Vec<Rect> = Vec::new();
    let mut row_y1 = 0u32;
    for r in rects {
        if row.is_empty() {
            row_y1 = r.y1;
            row.push(r);
            continue;
        }
        if r.y0 <= row_y1 {
            row_y1 = row_y1.max(r.y1);
            row.push(r);
        } else {
            row.sort_by_key(|r| (r.x0, r.y0));
            out.append(&mut row);
            row_y1 = r.y1;
            row.push(r);
        }
    }
    row.sort_by_key(|r| (r.x0, r.y0));
    out.append(&mut row);
    out
}

/// 矩形内の水平投影プロファイルから、インクのある行の連続区間(両端含む)を返す。
fn line_runs(ink: &GrayImage, rect: &Rect) -> Vec<(u32, u32)> {
    let mut runs = Vec::new();
    let mut start: Option<u32> = None;
    for y in rect.y0..=rect.y1 {
        let mut has = false;
        for x in rect.x0..=rect.x1 {
            if ink.get_pixel(x, y)[0] != 0 {
                has = true;
                break;
            }
        }
        match (has, start) {
            (true, None) => start = Some(y),
            (false, Some(s)) => {
                runs.push((s, y - 1));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, rect.y1));
    }
    runs
}

/// 矩形内のインク画素数。
fn ink_count(ink: &GrayImage, rect: &Rect) -> u64 {
    let mut n = 0u64;
    for y in rect.y0..=rect.y1 {
        for x in rect.x0..=rect.x1 {
            if ink.get_pixel(x, y)[0] != 0 {
                n += 1;
            }
        }
    }
    n
}

/// 中央値(偶数個なら上側)。空なら 0。
fn median(values: &mut [u32]) -> u32 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

/// 作業解像度の長さを原寸へ戻す(四捨五入、最小 0)。
fn scale_len(len: u32, work: u32, orig: u32) -> u32 {
    if work == 0 {
        return len;
    }
    ((len as u64 * orig as u64 + work as u64 / 2) / work as u64) as u32
}

/// 作業解像度の矩形を原寸座標へ戻す(画像内にクランプ)。
fn to_original(rect: &Rect, w: u32, h: u32, orig_w: u32, orig_h: u32) -> BlockRect {
    // 始点は「その作業画素が覆う原寸画素の先頭」。
    let start = |v: u32, work: u32, orig: u32| -> u32 {
        if work == 0 {
            return 0;
        }
        ((v as u64 * orig as u64) / work as u64).min(orig as u64 - 1) as u32
    };
    // 終点は包含端の**次**の作業画素を写した「排他端」。原寸の幅そのものまで許す
    // (`start` と違って orig-1 でクランプしない。右端に接するブロックの最後の
    // 1 画素が落ちるため)。
    let end = |v: u32, work: u32, orig: u32| -> u32 {
        if work == 0 {
            return orig;
        }
        ((v as u64 * orig as u64) / work as u64).min(orig as u64) as u32
    };
    let x0 = start(rect.x0, w, orig_w);
    let y0 = start(rect.y0, h, orig_h);
    let x1 = end(rect.x1 + 1, w, orig_w).max(x0 + 1);
    let y1 = end(rect.y1 + 1, h, orig_h).max(y0 + 1);
    // 幅・高さは「排他端 − 始点」。ここでさらに +1 すると全ブロックが 1px 大きくなる。
    BlockRect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    }
}

/// 行区間を原寸へ直し、重なりを潰して y 昇順の非重複列にする(帯境界の候補)。
fn normalized_rows(mut rows: Vec<(u32, u32)>, h: u32, orig_h: u32) -> Vec<(u32, u32)> {
    for r in rows.iter_mut() {
        let y0 = ((r.0 as u64 * orig_h as u64) / h.max(1) as u64) as u32;
        let y1 = ((((r.1 as u64 + 1) * orig_h as u64) / h.max(1) as u64) as u32).max(y0 + 1) - 1;
        *r = (y0.min(orig_h - 1), y1.min(orig_h - 1));
    }
    rows.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(rows.len());
    for r in rows {
        match out.last_mut() {
            Some(last) if r.0 <= last.1 => last.1 = last.1.max(r.1),
            _ => out.push(r),
        }
    }
    out
}

/// 読みやすさと帯分割を組み立てる。
///
/// 切り方は 3 通り([`Legibility::strategy`] で返す):
///
/// - `whole`: 画像全体を長辺 1568 で見れば行高 16px 以上 → 切らない
/// - `bands`: 縦が律速 → 横帯に割る(帯の境界は行間に合わせ、1 行重ねる)
/// - `blocks`: **横幅が律速**(帯を 1 本にしても 1568 換算で行高 < 16px)→
///   横帯では救えないので、ブロック単位の切り出しを返す([`block_crops`])
fn legibility_of(
    median_line_orig: u32,
    orig_w: u32,
    orig_h: u32,
    rows: &[(u32, u32)],
    blocks: &[TextBlock],
    warnings: &mut Vec<String>,
) -> Legibility {
    let long = orig_w.max(orig_h) as u64;
    let line_height_at_1568 =
        round3(median_line_orig as f64 * LEGIBLE_LONG_EDGE as f64 / long as f64);

    let whole = || {
        vec![SuggestedCrop {
            op: "crop".to_string(),
            rect: BlockRect {
                x: 0,
                y: 0,
                width: orig_w,
                height: orig_h,
            },
        }]
    };

    // 帯の長辺がこれ以下なら、長辺 1568 のプレビューで行高 >= 16px になる。
    let allowed = LEGIBLE_LONG_EDGE * median_line_orig as u64 / MIN_LEGIBLE_LINE_HEIGHT;
    if long <= allowed {
        // 画像全体で既に十分。
        return Legibility {
            strategy: STRATEGY_WHOLE.to_string(),
            line_height_at_1568_px: line_height_at_1568,
            recommended_bands: whole(),
        };
    }
    // 横幅が律速: 幅いっぱいの帯を 1 本にしても 1568 換算で行高が 16px に届かない。
    // 横帯を返しても次の一手にならないので、ブロック単位の切り出しに切り替える。
    if orig_w as u64 > allowed {
        let at_width = round3(LEGIBLE_LONG_EDGE as f64 * median_line_orig as f64 / orig_w as f64);
        // 切り出しの警告(16 本で足りない場合)は、下の説明の**後**に並べたいので
        // いったん別の Vec で受ける。
        let mut crop_warnings = Vec::new();
        let crops = block_crops(
            blocks,
            median_line_orig,
            orig_w,
            orig_h,
            allowed,
            &mut crop_warnings,
        );
        if !crops.is_empty() {
            warnings.push(format!(
                "text is small relative to the image width: even a full-width band renders lines \
                 at about {at_width:.1}px at long_edge 1568 (below 16px); read the {} block crops \
                 in recommended_bands (strategy \"blocks\") instead of full-width bands",
                crops.len()
            ));
            warnings.extend(crop_warnings);
            return Legibility {
                strategy: STRATEGY_BLOCKS.to_string(),
                line_height_at_1568_px: line_height_at_1568,
                recommended_bands: crops,
            };
        }
        warnings.push(format!(
            "text is small relative to the image width: even a full-width band renders lines at \
             about {at_width:.1}px at long_edge 1568 (below 16px); crop horizontally as well"
        ));
    }
    let band_h = allowed.clamp(1, orig_h as u64) as u32;
    if band_h >= orig_h {
        return Legibility {
            strategy: STRATEGY_WHOLE.to_string(),
            line_height_at_1568_px: line_height_at_1568,
            recommended_bands: whole(),
        };
    }

    let mut bands: Vec<SuggestedCrop> = Vec::new();
    let mut start = 0u32;
    while bands.len() < MAX_BANDS {
        let ideal_end = (start as u64 + band_h as u64).min(orig_h as u64) as u32; // exclusive
                                                                                  // この帯より下にまだ行が残っているか。残っていなければこれが最後の帯。
        let text_below = rows.iter().any(|r| r.1 >= ideal_end);
        // 帯の下端を「行の間」に合わせる(次の帯と 1 行重ねるため、帯に完全に
        // 収まった最後の行を覚えておく)。
        let last_inside = rows
            .iter()
            .enumerate()
            .rfind(|(_, r)| r.1 < ideal_end && r.0 >= start)
            .map(|(i, r)| (i, *r));
        let mut end = ideal_end;
        if text_below {
            if let Some((i, r)) = last_inside {
                // 境界は必ず理想の下端以下に丸める(帯が上限より高くならないように)。
                end = match rows.get(i + 1) {
                    Some(next) if next.0 > r.1 => (r.1 + (next.0 - r.1) / 2 + 1).min(ideal_end),
                    _ => (r.1 + 1).min(ideal_end),
                };
            }
            if end <= start {
                end = ideal_end;
            }
        }
        bands.push(SuggestedCrop {
            op: "crop".to_string(),
            rect: BlockRect {
                x: 0,
                y: start,
                width: orig_w,
                height: end - start,
            },
        });
        if !text_below || end >= orig_h {
            break;
        }
        // 次の帯は「前の帯の最後の行」から始める(1 行分の重なり)。
        let next_start = match last_inside {
            Some((_, r)) if r.0 > start => r.0,
            _ => end,
        };
        start = if next_start > start { next_start } else { end };
    }

    // 16 本(MAX_BANDS)で打ち切った場合、まだ下に文字が残っていることがある。
    // 黙って切ると「返された帯を全部読めば全文読める」と誤解できるので、
    // どこまで覆えたかと次の一手を英語で告げる(文言はテストで固定)。
    let covered_to = bands.last().map(|b| b.rect.y + b.rect.height).unwrap_or(0);
    let text_end = rows.last().map(|r| r.1 + 1).unwrap_or(0);
    if covered_to < text_end {
        warnings.push(format!(
            "recommended_bands cover only the first {covered_to} px of {text_end} px of text; \
             re-run detect_text_blocks on a crop of the remainder"
        ));
    }

    Legibility {
        strategy: STRATEGY_BANDS.to_string(),
        line_height_at_1568_px: line_height_at_1568,
        recommended_bands: bands,
    }
}

/// ブロック単位の切り出し(横幅が律速のときの `recommended_bands`)。
///
/// 横に広い写真(実測: 4080x3072 のメニュー、行高 13px = 1568 換算 5.0px)では、
/// 横帯をいくら細くしても幅 4080 が縮小率を決めてしまうので文字は読めない。
/// そこで「縮小後に読める大きさで収まる矩形」を並べる。
///
/// 手順(すべて整数演算 = 決定論):
///
/// 1. 各ブロックの rect に行高 1 行分(`pad`)の余白を付け、画像内へクランプ
/// 2. 読み順に走査し、隣接ブロックの和が `allowed`(= 長辺がこれ以下なら
///    1568 換算で行高 >= 16px)に収まる限りまとめる
/// 3. それでも長辺が `allowed` を超える矩形(1 ブロックが広すぎる場合)は、
///    一辺 `allowed` 以下のタイルへ読み順(上→下、左→右)に分割する
/// 4. [`MAX_BANDS`] 本を超えたら**面積の大きい順**に 16 本まで残し(同面積は読み順)、
///    返すときは読み順に戻す。落とした分は警告で告げる
fn block_crops(
    blocks: &[TextBlock],
    pad: u32,
    orig_w: u32,
    orig_h: u32,
    allowed: u64,
    warnings: &mut Vec<String>,
) -> Vec<SuggestedCrop> {
    if blocks.is_empty() || allowed == 0 {
        return Vec::new();
    }
    let limit = allowed.min(orig_w.max(orig_h) as u64) as u32;
    let limit = limit.max(1);

    // 1. 余白付け + クランプ。
    let padded: Vec<BlockRect> = blocks
        .iter()
        .map(|b| {
            let x0 = b.rect.x.saturating_sub(pad);
            let y0 = b.rect.y.saturating_sub(pad);
            let x1 = (b.rect.x + b.rect.width + pad).min(orig_w);
            let y1 = (b.rect.y + b.rect.height + pad).min(orig_h);
            BlockRect {
                x: x0,
                y: y0,
                width: x1.saturating_sub(x0).max(1),
                height: y1.saturating_sub(y0).max(1),
            }
        })
        .collect();

    // 2. 読み順のまま、収まる限り隣とまとめる。
    let mut groups: Vec<BlockRect> = Vec::new();
    for r in padded {
        if let Some(last) = groups.last_mut() {
            let u = union_rect(last, &r);
            if u.width.max(u.height) as u64 <= limit as u64 {
                *last = u;
                continue;
            }
        }
        groups.push(r);
    }

    // 3. 1 つで収まらない矩形はタイルに割る。
    let mut tiles: Vec<BlockRect> = Vec::new();
    for g in groups {
        if g.width.max(g.height) <= limit {
            tiles.push(g);
            continue;
        }
        let mut y = g.y;
        while y < g.y + g.height {
            let h = limit.min(g.y + g.height - y);
            let mut x = g.x;
            while x < g.x + g.width {
                let w = limit.min(g.x + g.width - x);
                tiles.push(BlockRect {
                    x,
                    y,
                    width: w,
                    height: h,
                });
                x += w;
            }
            y += h;
        }
    }

    // 4. 16 本に収まらなければ面積の大きい順に残し、読み順へ戻す。
    let total = tiles.len();
    if total > MAX_BANDS {
        let mut indexed: Vec<(usize, BlockRect)> = tiles.into_iter().enumerate().collect();
        indexed.sort_by_key(|(i, r)| (std::cmp::Reverse(r.width as u64 * r.height as u64), *i));
        indexed.truncate(MAX_BANDS);
        indexed.sort_by_key(|(i, _)| *i);
        tiles = indexed.into_iter().map(|(_, r)| r).collect();
        warnings.push(format!(
            "recommended_bands cover only the {MAX_BANDS} largest of {total} text crops; \
             re-run detect_text_blocks on a crop of the remainder"
        ));
    }

    tiles
        .into_iter()
        .map(|rect| SuggestedCrop {
            op: "crop".to_string(),
            rect,
        })
        .collect()
}

/// 2 つの原寸矩形の外接矩形。
fn union_rect(a: &BlockRect, b: &BlockRect) -> BlockRect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.width).max(b.x + b.width);
    let y1 = (a.y + a.height).max(b.y + b.height);
    BlockRect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: u32, y0: u32, x1: u32, y1: u32) -> Rect {
        Rect { x0, y0, x1, y1 }
    }

    #[test]
    fn median_picks_upper_middle() {
        assert_eq!(median(&mut [5, 1, 3]), 3);
        assert_eq!(median(&mut [4, 1, 3, 2]), 3);
        assert_eq!(median(&mut []), 0);
    }

    #[test]
    fn y_gap_is_zero_when_overlapping() {
        assert_eq!(rect(0, 0, 10, 10).y_gap(&rect(0, 5, 10, 20)), 0);
        assert_eq!(rect(0, 0, 10, 10).y_gap(&rect(0, 11, 10, 20)), 0);
        assert_eq!(rect(0, 0, 10, 10).y_gap(&rect(0, 16, 10, 20)), 5);
    }

    #[test]
    fn merge_threshold_uses_the_larger_of_height_and_gap() {
        // 行高 10、ギャップ 30 の 3 行 → ギャップ側(30 * 1.5 = 45)が効く。
        let comps = vec![
            rect(0, 0, 100, 9),
            rect(0, 40, 100, 49),
            rect(0, 80, 100, 89),
        ];
        assert_eq!(merge_threshold(&comps), 45);
        // 行高 20、ギャップ 2 → 行高側(20 * 1.5 = 30)が効く。
        let comps = vec![rect(0, 0, 100, 19), rect(0, 22, 100, 41)];
        assert_eq!(merge_threshold(&comps), 30);
    }

    #[test]
    fn reading_order_groups_overlapping_rows_left_to_right() {
        let out = reading_order(vec![
            rect(50, 0, 90, 20),
            rect(0, 0, 40, 20),
            rect(0, 100, 90, 120),
        ]);
        assert_eq!(
            out.iter().map(|r| (r.x0, r.y0)).collect::<Vec<_>>(),
            vec![(0, 0), (50, 0), (0, 100)]
        );
    }

    /// ブロック切り出しが 16 本を超えるときは、面積の大きい順に 16 本へ切って
    /// 残りを名指しする警告を出す(黙って切らない)。
    #[test]
    fn block_crops_are_capped_at_sixteen_with_a_warning() {
        // 100x50 のブロックを縦に 20 個(200px 間隔)。余白 10px、収まる上限 120px。
        // 縦に離れているのでまとめられず、20 本の切り出し候補になる。
        let blocks: Vec<TextBlock> = (0..20u32)
            .map(|i| TextBlock {
                rect: BlockRect {
                    x: 10,
                    y: i * 200 + 10,
                    width: 100,
                    height: 50,
                },
                line_count: 3,
                median_line_height_px: 10,
                ink_ratio: 0.3,
            })
            .collect();
        let mut warnings = Vec::new();
        let crops = block_crops(&blocks, 10, 200, 4000, 120, &mut warnings);
        assert_eq!(crops.len(), MAX_BANDS);
        // 面積が同じなので読み順の先頭 16 本が残る。
        assert_eq!(crops[0].rect.y, 0);
        assert!(crops.windows(2).all(|w| w[0].rect.y < w[1].rect.y));
        for c in &crops {
            assert_eq!(c.op, "crop");
            assert!(c.rect.width.max(c.rect.height) <= 120, "{:?}", c.rect);
        }
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("recommended_bands cover only the 16 largest of 20")),
            "{warnings:?}"
        );
    }

    #[test]
    fn smearing_fills_only_small_gaps() {
        let mut ink = GrayImage::new(20, 1);
        ink.put_pixel(0, 0, Luma([255]));
        ink.put_pixel(3, 0, Luma([255]));
        ink.put_pixel(13, 0, Luma([255]));
        let out = smear_horizontal(&ink, 2);
        // 0 と 3 の間(2px)は塗られ、3 と 13 の間(9px)は残る。
        assert_eq!(out.get_pixel(1, 0)[0], 255);
        assert_eq!(out.get_pixel(2, 0)[0], 255);
        assert_eq!(out.get_pixel(8, 0)[0], 0);
    }
}
