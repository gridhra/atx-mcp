//! 決定論的変換エンジン: bytes in → bytes out。
//! 同一入力バイト列 + 同一レシピ → バイト同一出力(ゴールデンテストで回帰検証する)。

use std::collections::BTreeMap;
use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use serde::Serialize;

use crate::codec;
use crate::linear::{pad_to_linear, pad_to_srgb, LinearImage, Space};
use crate::pixel_ops;
use crate::recipe::{
    parse_aspect_ratio, parse_hex_color, CoordinateSpace, FlipDirection, Operation, OutputFormat,
    StripScope, TransformRecipe,
};
use crate::transform::{map_source_rect, Affine};
use crate::{AtxError, Limits, Result};

/// エンジンの挙動バージョン。出力バイト列に影響する変更を入れたら上げる。
/// ゴールデンテストはこのバージョンの挙動をピン留めしている。
///
/// `atx-core/2`(v0.4): 内部表現を RGBA8 / sRGB から **f32 リニアライト**へ移行した
/// 唯一の破壊的リリース。レシピの正規化 JSON とハッシュは 1 ビットも変わっていないが、
/// 出力バイト列は全面的に変わる(DESIGN.md §9.5)。
pub const ENGINE_VERSION: &str = "atx-core/2";

/// 既定のパディング色(白・不透明)。
const DEFAULT_PAD: [u8; 4] = [255, 255, 255, 255];

/// inspect 結果。MCP の structuredContent にそのまま載せられる形。
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub mime_type: String,
    pub byte_size: u64,
    /// EXIF Orientation タグ値(1-8)。存在しなければ None。
    pub exif_orientation: Option<u16>,
    /// Orientation 適用後の実効寸法。
    pub oriented_width: u32,
    pub oriented_height: u32,
    pub has_alpha: bool,
    /// ICC プロファイルの有無。
    pub has_icc_profile: bool,
    /// GPS EXIF の有無(PII 警告用)。
    pub has_gps: bool,
    /// 主要 EXIF の要約(撮影日時、カメラ等)。キーは小文字 snake_case。
    pub exif_summary: std::collections::BTreeMap<String, String>,
}

/// 変換結果。
#[derive(Debug, Clone)]
pub struct EncodedOutput {
    pub bytes: Vec<u8>,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    /// 実行時警告(例: "rotation crop removed 3.2% of pixels")。
    pub warnings: Vec<String>,
}

/// EXIF から読み取ったメタデータ。
struct ExifInfo {
    orientation: Option<u16>,
    has_gps: bool,
    has_any: bool,
    summary: BTreeMap<String, String>,
}

/// 入力バイト列を検査する(デコードは寸法確認まで、limits 適用)。
///
/// 検査順序は「バイトサイズ → フォーマット判定(マジックバイト)→ 寸法」。
/// 画素データのフルデコードは行わないため、デコード爆弾に対しても安全。
pub fn inspect_bytes(bytes: &[u8], limits: &Limits) -> Result<ImageInfo> {
    check_byte_limit(bytes, limits)?;

    let reader = build_reader(bytes, limits)?;
    let format = reader.format();
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| AtxError::Decode(e.to_string()))?;
    let (width, height) = decoder.dimensions();
    check_pixel_limit(width, height, limits)?;

    let has_alpha = decoder.color_type().has_alpha();
    let icc = decoder.icc_profile().ok().flatten();
    let exif = read_exif(bytes);
    let orientation = exif.orientation.unwrap_or(1);
    let (oriented_width, oriented_height) =
        pixel_ops::oriented_dimensions(width, height, orientation);

    Ok(ImageInfo {
        width,
        height,
        mime_type: format
            .map(|f| f.to_mime_type().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string()),
        byte_size: bytes.len() as u64,
        exif_orientation: exif.orientation,
        oriented_width,
        oriented_height,
        has_alpha,
        has_icc_profile: icc.is_some_and(|p| !p.is_empty()),
        has_gps: exif.has_gps,
        exif_summary: exif.summary,
    })
}

/// レシピを適用し、エンコード済みバイト列を返す。
///
/// - 適用前に `recipe::validate` を通す
/// - `Encode` op が無い場合は入力と同じフォーマットで再エンコード
///   (エンコード非対応フォーマットが入力だった場合は JPEG にフォールバックし warning を出す)
///
/// # メタデータの実際の挙動(v1)
///
/// - **Orientation**: デコード直後に必ず画素へ焼き込んで正規化する
///   (EXIF を落とす以上、これを条件付きにすると出力の向きが壊れるため)。
///   したがって `AutoOrient` op は明示的な意思表示としては有効だが、実質 no-op である。
/// - **EXIF**: 再エンコード時に**常に破棄する**(v1 の割り切り)。入力に EXIF があった場合は
///   `EncodedOutput::warnings` にその旨を記録する。
/// - **ICC**: 既定では温存する。ただし埋め込みに対応しているのは JPEG 出力のみで、
///   PNG / WebP / AVIF 出力では破棄され warning を出す。
/// - **`StripMetadata { scope: All }`**: EXIF に加えて ICC も破棄し、
///   出力に一切のメタデータが載らないことを保証する。
/// - **`StripMetadata { scope: Gps }`**: v1 では EXIF 全体が既に破棄されるため既定動作と同じ
///   (GPS を含む上位集合を落とす)。warning でその旨を明示する。
/// - **`StripMetadata { scope: Exif }`**: EXIF(GPS 含む)が確実に無いことを保証しつつ、
///   **ICC は温存する**(Web 配信で色を動かさないため)。ICC を実際に埋め込めるのは
///   JPEG 出力のみという既定の制約はそのまま。
///
/// # SOURCE 座標系のクロップ
///
/// `Crop { rect, coordinate_space: Source }` のために、エンジンは
/// 「**入力画像(EXIF orientation 正規化前)の画素座標 → 現在のパイプライン座標**」の
/// 2D アフィン変換を保持し、幾何 op ごとに合成していく
/// (orientation 正規化 / rotate / crop / pad / resize / flip / perspective。
/// adjust・encode・strip は座標を動かさない)。
/// 詳細は `crate::transform` と `recipe::CoordinateSpace` を参照。
/// レシピが参照する他アセット(LUT 等)の実体を解決する。
/// atx-core はストア実装に依存しないため、呼び出し側(atx-mcp / CLI / テスト)が実装する。
pub trait AssetResolver {
    /// revision id の実体バイト列を返す。見つからなければ Err。
    fn read_revision(&self, revision_id: &str) -> Result<Vec<u8>>;
}

/// アセット参照を一切解決できないリゾルバ(後方互換の既定)。
/// `lut` 等のアセット参照 op を含むレシピはエラーになる。
pub struct NoAssets;

impl AssetResolver for NoAssets {
    fn read_revision(&self, revision_id: &str) -> Result<Vec<u8>> {
        Err(AtxError::InvalidRecipe(format!(
            "this recipe references asset {revision_id}, but no asset resolver is available              in this context"
        )))
    }
}

pub fn apply_recipe(
    bytes: &[u8],
    recipe: &TransformRecipe,
    limits: &Limits,
) -> Result<EncodedOutput> {
    apply_recipe_with_assets(bytes, recipe, limits, &NoAssets)
}

/// アセット参照 op(`lut` 等)を含むレシピに対応した本体。
/// `assets` から参照先の実体を読み、決定論を保ったまま適用する。
pub fn apply_recipe_with_assets(
    bytes: &[u8],
    recipe: &TransformRecipe,
    limits: &Limits,
    assets: &dyn AssetResolver,
) -> Result<EncodedOutput> {
    crate::recipe::validate(recipe)?;
    check_byte_limit(bytes, limits)?;

    // 作業空間の遅延決定: **最初に現れる空間依存 op が要求する空間**へ直接デコードする。
    // こうすると「トーン系 op だけのレシピ」では伝達関数を一度も通さずに済み、
    // 符号値が u8 のビット精度のまま最後まで運ばれる(空間依存 op が無い
    // = クロップ/エンコードだけのレシピも同様に無損失)。
    //
    // ただし **layers があるときは常に sRGB 符号値空間**でデコードする。
    // 合成が sRGB 符号値空間で定義されている(`ops::blend` 参照)ためで、
    // u8 → sRGB f32 は伝達関数を通さない厳密な `/255` なので情報は落ちない。
    let initial_space = if recipe.layers.is_some() {
        Space::Srgb
    } else {
        recipe
            .operations
            .iter()
            .find_map(op_space)
            .unwrap_or(Space::Srgb)
    };
    let input = decode_normalized(bytes, limits, initial_space)?;
    let input_format = input.format;
    let mut icc = input.icc.clone();
    let exif_has_any = input.exif_has_any;

    // 1 回の apply_recipe 内でのマスク解決キャッシュ(同じ MaskRef + 同じ寸法は 1 回だけ)。
    let mut runner = OpRunner {
        assets,
        limits,
        masks: crate::ops::mask::MaskCache::new(),
    };

    // --- レイヤー合成(v0.6)/ 単一パイプライン(v1)---
    let mut st = match &recipe.layers {
        Some(layers) => composite_layers(&mut runner, layers, &input, limits)?,
        None => PipelineState {
            img: input.img,
            space: initial_space,
            xf: input.xf,
            warnings: Vec::new(),
            strip: None,
            has_alpha: input.has_alpha,
        },
    };

    // --- op を順次適用(layers があるときは合成結果への仕上げパス)---
    runner.run_ops(&mut st, &recipe.operations)?;

    let PipelineState {
        mut img,
        mut space,
        mut warnings,
        strip,
        has_alpha,
        ..
    } = st;

    // 出口の空間確定: 常に sRGB 符号値へ移してから量子化する。
    // 最後の op が sRGB 空間だった場合は変換が 0 回で済み、丸め直前に
    // 変換の補間誤差が乗らない(`linear::encoded_to_rgba8` のドキュメント参照)。
    ensure_space(&mut img, &mut space, Space::Srgb);

    // --- 出力フォーマットの決定 ---
    let encode_op = recipe.operations.iter().find_map(|op| match op {
        Operation::Encode {
            format,
            quality,
            bit_depth,
        } => Some((*format, *quality, *bit_depth)),
        _ => None,
    });
    let (format, quality, bit_depth) = match encode_op {
        Some(v) => v,
        None => match input_format.and_then(output_format_of) {
            Some(f) => (f, None, None),
            None => {
                warnings.push(format!(
                    "input format {:?} cannot be re-encoded; falling back to jpeg",
                    input_format
                ));
                (OutputFormat::Jpeg, None, None)
            }
        },
    };
    // validate が 8 / 16 のみを通し、16 は png のみに限定している。
    let bit_depth = bit_depth.unwrap_or(8);

    // --- メタデータの取り扱い ---
    if strip == Some(StripScope::All) && icc.is_some() {
        icc = None;
        warnings.push("strip_metadata(all): ICC profile removed".to_string());
    }
    if strip == Some(StripScope::Gps) {
        warnings.push(
            "strip_metadata(gps): v1 drops the entire EXIF block (a superset of GPS)".to_string(),
        );
    }
    if strip == Some(StripScope::Exif) {
        warnings.push(
            "strip_metadata(exif): EXIF (including GPS) is guaranteed absent; \
             the ICC profile is preserved"
                .to_string(),
        );
    }
    if exif_has_any {
        warnings.push(
            "EXIF metadata was dropped on re-encode; orientation is normalized into pixel data"
                .to_string(),
        );
    }
    if icc.is_some() && format != OutputFormat::Jpeg {
        warnings.push(format!(
            "ICC profile dropped: embedding is only supported for jpeg output in v1 (output: {:?})",
            format
        ));
        icc = None;
    }

    // --- エンコード ---
    let out_has_alpha = has_alpha && format != OutputFormat::Jpeg;
    let (out_bytes, icc_embedded) = codec::encode(
        &img,
        format,
        quality,
        out_has_alpha,
        icc.as_deref(),
        bit_depth,
    )?;
    if icc.is_some() && !icc_embedded {
        warnings.push("ICC profile could not be embedded and was dropped".to_string());
    }

    let (w, h) = img.dimensions();
    Ok(EncodedOutput {
        bytes: out_bytes,
        mime_type: codec::mime_type(format).to_string(),
        width: w,
        height: h,
        warnings,
    })
}

/// デコード済み入力(EXIF orientation 正規化済み)。
struct DecodedInput {
    img: LinearImage,
    /// SOURCE 画素座標 → 正規化後座標 のアフィン変換(orientation ぶん)。
    xf: Affine,
    has_alpha: bool,
    icc: Option<Vec<u8>>,
    format: Option<ImageFormat>,
    exif_has_any: bool,
}

/// バイト列を `space` の `LinearImage` へデコードし、EXIF orientation を画素へ焼き込む。
///
/// 入力画像とレイヤーソース(revision)の両方がこの同じ経路を通る
/// = レイヤーに載せた画像も向きが正規化される。
fn decode_normalized(bytes: &[u8], limits: &Limits, space: Space) -> Result<DecodedInput> {
    check_byte_limit(bytes, limits)?;

    let reader = build_reader(bytes, limits)?;
    let format = reader.format();
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| AtxError::Decode(e.to_string()))?;
    let (width, height) = decoder.dimensions();
    check_pixel_limit(width, height, limits)?;
    let has_alpha = decoder.color_type().has_alpha();
    let icc = decoder
        .icc_profile()
        .ok()
        .flatten()
        .filter(|p| !p.is_empty());
    let decoded =
        DynamicImage::from_decoder(decoder).map_err(|e| AtxError::Decode(e.to_string()))?;
    // 16bit 入力(PNG16 等)は 65536 エントリの EOTF テーブルで線形化する。
    // 8bit へ落としてから線形化すると、そもそも 16bit で受け取った意味が無くなる。
    let is_16bit = matches!(
        decoded.color(),
        image::ColorType::L16
            | image::ColorType::La16
            | image::ColorType::Rgb16
            | image::ColorType::Rgba16
    );
    let mut img = match (is_16bit, space) {
        (true, Space::Linear) => LinearImage::from_rgba16(&decoded.to_rgba16()),
        (true, Space::Srgb) => LinearImage::from_rgba16_srgb(&decoded.to_rgba16()),
        (false, Space::Linear) => LinearImage::from_rgba8(&decoded.to_rgba8()),
        (false, Space::Srgb) => LinearImage::from_rgba8_srgb(&decoded.to_rgba8()),
    };
    drop(decoded);

    // SOURCE 画素座標 → CURRENT パイプライン座標 のアフィン変換。
    // 幾何 op ごとに合成し、`Crop { coordinate_space: Source }` で使う。
    let mut xf = Affine::IDENTITY;

    // --- Orientation の正規化(常時) ---
    let exif = read_exif(bytes);
    if let Some(orientation) = exif.orientation {
        if orientation != 1 {
            let (w, h) = img.dimensions();
            xf = xf.then(pixel_ops::orientation_affine(w, h, orientation));
            img = pixel_ops::apply_orientation(img, orientation);
        }
    }

    Ok(DecodedInput {
        img,
        xf,
        has_alpha,
        icc,
        format,
        exif_has_any: exif.has_any,
    })
}

/// limits 検査つきの汎用ラスタデコード(色空間の解釈も orientation の正規化もしない)。
///
/// 入力画像とレイヤーソースは `decode_normalized` を通るが、マスク画像のように
/// 「向きも空間も関係なく画素だけが欲しい」経路も **同じ検査**(バイトサイズ →
/// フォーマット判定 → 寸法 → デコーダのアロケーション上限)を通す必要がある。
/// 検査を書き写さずに済むよう、両者の共通部分をここに 1 本だけ切り出してある。
pub(crate) fn decode_checked(bytes: &[u8], limits: &Limits) -> Result<DynamicImage> {
    check_byte_limit(bytes, limits)?;
    let reader = build_reader(bytes, limits)?;
    let decoder = reader
        .into_decoder()
        .map_err(|e| AtxError::Decode(e.to_string()))?;
    let (width, height) = decoder.dimensions();
    check_pixel_limit(width, height, limits)?;
    DynamicImage::from_decoder(decoder).map_err(|e| AtxError::Decode(e.to_string()))
}

/// パイプラインの可変状態(単一パイプラインでも、レイヤー内のネストパイプラインでも同じ)。
struct PipelineState {
    img: LinearImage,
    space: Space,
    xf: Affine,
    warnings: Vec<String>,
    strip: Option<StripScope>,
    has_alpha: bool,
}

/// op ループの実行器。アセット解決とマスクキャッシュを全パイプラインで共有する
/// (同じ `MaskRef` + 同じ寸法はレイヤーをまたいでも 1 回しか解決しない)。
struct OpRunner<'a> {
    assets: &'a dyn AssetResolver,
    /// マスク画像のデコードにも入力画像と同じ上限を適用する。
    limits: &'a Limits,
    masks: crate::ops::mask::MaskCache,
}

/// レイヤー列を合成し、仕上げパスの入力となるキャンバスを返す(v0.6)。
///
/// - 先頭レイヤーが backdrop。以降のレイヤーは backdrop と**同寸法**でなければならない
/// - 各レイヤーは自分のソース画素に自分の `ops` を適用してから合成される
/// - 合成は sRGB 符号値空間・ストレートアルファ(`ops::blend`)
/// - 仕上げパスへ引き継ぐアフィン変換は **backdrop レイヤーのもの**
///   (キャンバスの幾何は backdrop が決めるため)
fn composite_layers(
    runner: &mut OpRunner,
    layers: &[crate::recipe::Layer],
    input: &DecodedInput,
    limits: &Limits,
) -> Result<PipelineState> {
    use crate::recipe::LayerSource;

    let mut canvas: Option<PipelineState> = None;
    for (i, layer) in layers.iter().enumerate() {
        // --- ソース画素 ---
        let (img, xf, src_alpha) = match &layer.source {
            LayerSource::Base(_) => (input.img.clone(), input.xf, input.has_alpha),
            LayerSource::Revision { revision_id } => {
                let bytes = runner.assets.read_revision(revision_id).map_err(|e| {
                    AtxError::InvalidRecipe(format!(
                        "layers[{i}] (source): revision {revision_id} could not be read: {e}"
                    ))
                })?;
                let decoded = decode_normalized(&bytes, limits, Space::Srgb).map_err(|e| {
                    AtxError::InvalidRecipe(format!(
                        "layers[{i}] (source): revision {revision_id} could not be decoded: {e}"
                    ))
                })?;
                (decoded.img, decoded.xf, decoded.has_alpha)
            }
        };

        // --- レイヤー内のネストパイプライン ---
        let mut st = PipelineState {
            img,
            space: Space::Srgb,
            xf,
            warnings: Vec::new(),
            strip: None,
            has_alpha: src_alpha,
        };
        runner
            .run_ops(&mut st, &layer.ops)
            .map_err(|e| layer_error(i, e))?;
        // 合成は sRGB 符号値空間で行う。
        ensure_space(&mut st.img, &mut st.space, Space::Srgb);
        let warnings: Vec<String> = st
            .warnings
            .drain(..)
            .map(|w| format!("layers[{i}]: {w}"))
            .collect();
        st.warnings = warnings;

        // --- 合成 ---
        match canvas {
            None => canvas = Some(st),
            Some(ref mut cv) => {
                let (bw, bh) = cv.img.dimensions();
                let (lw, lh) = st.img.dimensions();
                if (lw, lh) != (bw, bh) {
                    return Err(AtxError::InvalidRecipe(format!(
                        "layers[{i}]: after its ops the layer is {lw}x{lh} but the backdrop \
                         (layers[0]) is {bw}x{bh}; every layer must match the backdrop \
                         dimensions — add a resize or crop to layers[{i}].ops to bring it to \
                         {bw}x{bh}"
                    )));
                }
                let weights = match &layer.mask {
                    Some(mask) => Some(
                        runner
                            .masks
                            .resolve(mask, bw, bh, runner.assets, runner.limits)
                            .map_err(|e| layer_error(i, e))?
                            .to_vec(),
                    ),
                    None => None,
                };
                crate::ops::blend::composite(
                    &mut cv.img,
                    &st.img,
                    layer.blend_mode,
                    layer.opacity as f32,
                    weights.as_deref(),
                );
                cv.has_alpha = cv.has_alpha || st.has_alpha;
                cv.warnings.extend(st.warnings);
            }
        }
    }
    // validate がレイヤー非空を保証している。
    canvas.ok_or_else(|| AtxError::InvalidRecipe("layers must not be empty".into()))
}

/// レイヤー内で出たエラーにレイヤー番号を付ける。
fn layer_error(i: usize, e: AtxError) -> AtxError {
    match e {
        AtxError::Operation { index, op, message } => AtxError::Operation {
            index,
            op,
            message: format!("in layers[{i}]: {message}"),
        },
        AtxError::InvalidRecipe(m) => AtxError::InvalidRecipe(format!("layers[{i}]: {m}")),
        other => other,
    }
}

impl OpRunner<'_> {
    /// op 列を順に適用する(単一パイプラインとレイヤー内で完全に同じコード)。
    fn run_ops(&mut self, st: &mut PipelineState, ops: &[Operation]) -> Result<()> {
        for (index, op) in ops.iter().enumerate() {
            let fail = |message: String| AtxError::Operation {
                index,
                op: op_name(op).to_string(),
                message,
            };

            // --- 局所適用マスク(v0.5): op ループ 1 箇所だけの汎用処理 ---
            //
            // マスクが付いていれば、**その op の作業空間へ先に移してから**適用前の状態を
            // 退避しておく(空間変換を挟んだ後の値どうしを混ぜないと意味が変わる)。
            // op 本体の `ensure_space` はここで既に目的の空間なので no-op になる。
            let masked = op.mask().cloned();
            let before = if masked.is_some() {
                if let Some(want) = op_space(op) {
                    ensure_space(&mut st.img, &mut st.space, want);
                }
                Some(st.img.clone())
            } else {
                None
            };

            match op {
                // Orientation はデコード直後に正規化済みのため、ここでは何もしない。
                Operation::AutoOrient => {}
                Operation::Rotate {
                    angle_degrees,
                    crop,
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Linear);
                    let (rotated, warning, step) = pixel_ops::rotate(
                        &st.img,
                        *angle_degrees,
                        *crop,
                        pad_to_linear(DEFAULT_PAD),
                    );
                    st.img = rotated;
                    st.xf = st.xf.then(step);
                    if let Some(w) = warning {
                        st.warnings.push(w);
                    }
                }
                Operation::Crop {
                    aspect_ratio,
                    rect,
                    anchor,
                    mode,
                    pad_color,
                    coordinate_space,
                } => {
                    let pad_u8 = match pad_color {
                        Some(c) => parse_hex_color(c)
                            .ok_or_else(|| fail(format!("invalid pad_color {c:?}")))?,
                        None => DEFAULT_PAD,
                    };
                    // crop / pad は添字操作なので作業空間を選ばない(v2 では余計な
                    // 空間往復を避けるため、現在の空間のまま実行する)。pad 色だけを
                    // 現在の空間へ写す。
                    let pad = match st.space {
                        Space::Linear => pad_to_linear(pad_u8),
                        Space::Srgb => pad_to_srgb(pad_u8),
                    };
                    if let Some(ratio) = aspect_ratio {
                        let ratio = parse_aspect_ratio(ratio)
                            .ok_or_else(|| fail(format!("invalid aspect_ratio {ratio:?}")))?;
                        let (fitted, step) =
                            pixel_ops::fit_aspect(&st.img, ratio, *anchor, *mode, pad);
                        st.img = fitted;
                        st.xf = st.xf.then(step);
                        if *mode == crate::recipe::CropMode::Pad && pad_u8[3] < 255 {
                            st.has_alpha = true;
                        }
                    } else if let Some(rect) = rect {
                        // source 座標指定なら、ここまでの幾何変換で矩形を現在の座標系へ写す。
                        let effective = match coordinate_space {
                            CoordinateSpace::Current => *rect,
                            CoordinateSpace::Source => {
                                let (cw, ch) = st.img.dimensions();
                                let mapped =
                                    map_source_rect(&st.xf, *rect, cw, ch).map_err(fail)?;
                                if mapped.clamped {
                                    st.warnings.push(format!(
                                        "operations[{index}] (crop): source-space rect \
                                     {}x{}+{}+{} mapped to [{}, {}]x[{}, {}] and was clamped to \
                                     {}x{}+{}+{} inside the current {cw}x{ch} image",
                                        rect.width,
                                        rect.height,
                                        rect.x,
                                        rect.y,
                                        mapped.raw.0,
                                        mapped.raw.2,
                                        mapped.raw.1,
                                        mapped.raw.3,
                                        mapped.rect.width,
                                        mapped.rect.height,
                                        mapped.rect.x,
                                        mapped.rect.y,
                                    ));
                                }
                                mapped.rect
                            }
                        };
                        st.img = pixel_ops::crop_rect(&st.img, effective).map_err(fail)?;
                        st.xf = st.xf.then(Affine::translate(
                            -(effective.x as f64),
                            -(effective.y as f64),
                        ));
                    }
                }
                Operation::Resize {
                    width,
                    height,
                    fit,
                    without_enlargement,
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Linear);
                    let (iw, ih) = st.img.dimensions();
                    let ((sw, sh), (cw, ch)) = pixel_ops::resize_targets(
                        iw,
                        ih,
                        *width,
                        *height,
                        *fit,
                        *without_enlargement,
                    );
                    st.img = pixel_ops::resize_lanczos3(&st.img, sw, sh).map_err(fail)?;
                    // 連続座標では拡縮は原点固定の純粋なスケール。
                    st.xf = st
                        .xf
                        .then(Affine::scale(sw as f64 / iw as f64, sh as f64 / ih as f64));
                    if (cw, ch) != (sw, sh) {
                        let x = (sw - cw) / 2;
                        let y = (sh - ch) / 2;
                        st.img = pixel_ops::crop_view(&st.img, x, y, cw, ch);
                        // fit=cover の内部中央クロップぶんの平行移動。
                        st.xf = st.xf.then(Affine::translate(-(x as f64), -(y as f64)));
                    }
                }
                Operation::Adjust {
                    brightness,
                    contrast,
                    saturation,
                    sharpness,
                    ..
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Srgb);
                    st.img =
                        pixel_ops::adjust(&st.img, *brightness, *contrast, *saturation, *sharpness);
                }
                Operation::Perspective {
                    quad,
                    vertical_degrees,
                    horizontal_degrees,
                    pad_color,
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Linear);
                    let (out, warns, step) = crate::ops::perspective::apply(
                        &st.img,
                        quad,
                        vertical_degrees,
                        horizontal_degrees,
                        pad_color,
                    )
                    .map_err(|e| fail(e.to_string()))?;
                    st.img = out;
                    // 射影ステップ。これ以降 `coordinate_space: "source"` の矩形は
                    // 3x3 射影行列を経由して写像される(`crate::transform` 参照)。
                    st.xf = st.xf.then(step);
                    st.warnings.extend(
                        warns
                            .into_iter()
                            .map(|w| format!("operations[{index}] (perspective): {w}")),
                    );
                }
                Operation::ColorMatrix { matrix, .. } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Srgb);
                    st.img = crate::ops::color::color_matrix(&st.img, matrix);
                }
                Operation::Curves {
                    master,
                    red,
                    green,
                    blue,
                    ..
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Srgb);
                    st.img = crate::ops::color::curves(&st.img, master, red, green, blue);
                }
                Operation::Levels {
                    in_black,
                    in_white,
                    gamma,
                    out_black,
                    out_white,
                    ..
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Srgb);
                    st.img = crate::ops::color::levels(
                        &st.img, *in_black, *in_white, *gamma, *out_black, *out_white,
                    );
                }
                Operation::Flip { direction } => {
                    // flip も幾何 op なので座標変換へ畳み込む(畳み込まないと後続の
                    // `crop { coordinate_space: "source" }` が反転前の位置を切り出す)。
                    // 連続座標では反転は厳密に線形: 水平は `u' = w − u`、垂直は `v' = h − v`
                    // (EXIF orientation 2 / 4 の `pixel_ops::orientation_affine` と同じ形)。
                    let (w, h) = st.img.dimensions();
                    let step = match direction {
                        FlipDirection::Horizontal => {
                            Affine::linear(-1.0, 0.0, w as f64, 0.0, 1.0, 0.0)
                        }
                        FlipDirection::Vertical => {
                            Affine::linear(1.0, 0.0, 0.0, 0.0, -1.0, h as f64)
                        }
                    };
                    st.img = crate::ops::finish::flip(&st.img, *direction);
                    st.xf = st.xf.then(step);
                }
                Operation::Vignette {
                    strength,
                    radius,
                    feather,
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Linear);
                    st.img = crate::ops::finish::vignette(&st.img, *strength, *radius, *feather);
                }
                Operation::Grain {
                    amount,
                    size,
                    monochrome,
                    seed,
                    ..
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Srgb);
                    st.img = crate::ops::finish::grain(&st.img, *amount, *size, *monochrome, *seed);
                }
                Operation::GradientMap { stops, .. } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Srgb);
                    st.img = crate::ops::gradient::apply(&st.img, stops);
                }
                Operation::Pixelate { block_size, region } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Linear);
                    st.img =
                        crate::ops::pixelate::apply(&st.img, *block_size, region).map_err(fail)?;
                }
                Operation::AutoLevels {
                    clip_percent,
                    per_channel,
                    ..
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Srgb);
                    st.img = crate::ops::auto_levels::apply(&st.img, *clip_percent, *per_channel);
                }
                Operation::Blur { sigma, .. } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Linear);
                    st.img = crate::ops::blur::gaussian_blur(&st.img, *sigma);
                }
                Operation::Clone {
                    src_x,
                    src_y,
                    dest_x,
                    dest_y,
                    radius,
                    feather_px,
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Linear);
                    st.img = crate::ops::clone_heal::apply_clone(
                        &st.img,
                        *src_x,
                        *src_y,
                        *dest_x,
                        *dest_y,
                        *radius,
                        *feather_px,
                    )
                    .map_err(fail)?;
                }
                Operation::Heal {
                    src_x,
                    src_y,
                    dest_x,
                    dest_y,
                    radius,
                    feather_px,
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Linear);
                    st.img = crate::ops::clone_heal::apply_heal(
                        &st.img,
                        *src_x,
                        *src_y,
                        *dest_x,
                        *dest_y,
                        *radius,
                        *feather_px,
                    )
                    .map_err(fail)?;
                }
                Operation::SvgOverlay {
                    svg_revision_id,
                    x,
                    y,
                    width,
                    height,
                    opacity,
                    blend_mode,
                } => {
                    let svg_bytes = self
                        .assets
                        .read_revision(svg_revision_id)
                        .map_err(|e| fail(e.to_string()))?;
                    let raster =
                        crate::ops::svg::rasterize(&svg_bytes, *width, *height).map_err(fail)?;
                    // ラスタは sRGB 符号値・ストレートアルファなので、
                    // 合成もその空間で行う(`layers` と同じ判断。DESIGN.md §9.7)。
                    ensure_space(&mut st.img, &mut st.space, Space::Srgb);
                    crate::ops::svg::apply(
                        &mut st.img,
                        &raster.img,
                        *x,
                        *y,
                        *blend_mode,
                        *opacity as f32,
                    );
                    st.warnings.extend(
                        raster
                            .warnings
                            .into_iter()
                            .map(|w| format!("operations[{index}] (svg_overlay): {w}")),
                    );
                    // 出力アルファは変えない: 不透明な backdrop へ何を載せても
                    // αo = αs + αb(1 − αs) = 1 のままで、透明部分は元から
                    // `has_alpha` に反映されている。
                }
                Operation::Median { radius, .. } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Linear);
                    st.img = crate::ops::blur::median(&st.img, *radius);
                }
                Operation::UnsharpMask {
                    amount,
                    radius,
                    threshold,
                    ..
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Linear);
                    st.img = crate::ops::blur::unsharp_mask(&st.img, *amount, *radius, *threshold);
                }
                Operation::Lut {
                    lut_revision_id,
                    strength,
                    ..
                } => {
                    let lut_bytes = self
                        .assets
                        .read_revision(lut_revision_id)
                        .map_err(|e| fail(e.to_string()))?;
                    let text = std::str::from_utf8(&lut_bytes).map_err(|_| {
                        fail(format!("asset {lut_revision_id} is not a text .cube file"))
                    })?;
                    let lut = crate::ops::lut::parse_cube(text).map_err(|e| fail(e.to_string()))?;
                    ensure_space(&mut st.img, &mut st.space, Space::Srgb);
                    st.img = crate::ops::lut::apply(&st.img, &lut, *strength);
                }
                Operation::WhiteBalance {
                    temperature, tint, ..
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Linear);
                    st.img = crate::ops::wb::apply(&st.img, *temperature, *tint);
                }
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
                } => {
                    let bands = [
                        *red, *orange, *yellow, *green, *aqua, *blue, *purple, *magenta,
                    ];
                    ensure_space(&mut st.img, &mut st.space, Space::Srgb);
                    st.img = crate::ops::hsl::apply(&st.img, &bands);
                }
                Operation::Convolve {
                    kernel,
                    size,
                    divisor,
                    offset,
                    ..
                } => {
                    ensure_space(&mut st.img, &mut st.space, Space::Linear);
                    st.img = crate::ops::convolve::apply(&st.img, kernel, *size, *divisor, *offset);
                }
                Operation::StripMetadata { scope } => {
                    st.strip = Some(*scope);
                }
                // エンコード指定は最後にまとめて処理する(validate により最後の op であることが保証される)。
                Operation::Encode { .. } => {}
            }

            // --- マスクブレンド(現在の作業空間、RGBA 4 チャンネル) ---
            if let (Some(mask), Some(before)) = (masked, before) {
                let (w, h) = st.img.dimensions();
                if before.dimensions() != (w, h) {
                    return Err(fail(
                        "mask is only supported for ops that preserve the image dimensions"
                            .to_string(),
                    ));
                }
                let weights = self
                    .masks
                    .resolve(&mask, w, h, self.assets, self.limits)
                    // 上限超過は「op の失敗」ではなく入力ガードなので、そのまま返す
                    // (呼び出し側が LimitExceeded として区別できるように)。
                    .map_err(|e| match e {
                        limit @ AtxError::LimitExceeded(_) => limit,
                        other => fail(other.to_string()),
                    })?;
                st.img = crate::ops::mask::blend(&before, &st.img, weights);
            }
        }
        Ok(())
    }
}

/// op が要求する作業空間(`ops/mod.rs` の表)。
///
/// `None` は**空間非依存**の op。`auto_orient` / `crop` / `strip_metadata` / `encode` は
/// 添字操作かメタデータ操作でしかないので、どちらの空間でもビット同一に動く
/// (`crop` の pad 色だけは現在の空間へ写す。engine の該当分岐を参照)。
fn op_space(op: &Operation) -> Option<Space> {
    match op {
        Operation::AutoOrient
        | Operation::Crop { .. }
        | Operation::Flip { .. }
        | Operation::StripMetadata { .. }
        | Operation::Encode { .. } => None,
        Operation::Rotate { .. }
        | Operation::Resize { .. }
        | Operation::Perspective { .. }
        | Operation::Blur { .. }
        | Operation::Median { .. }
        | Operation::UnsharpMask { .. }
        | Operation::Convolve { .. }
        | Operation::Clone { .. }
        | Operation::Heal { .. }
        | Operation::Vignette { .. }
        | Operation::Pixelate { .. }
        | Operation::WhiteBalance { .. } => Some(Space::Linear),
        Operation::Adjust { .. }
        | Operation::ColorMatrix { .. }
        | Operation::Curves { .. }
        | Operation::Levels { .. }
        | Operation::Lut { .. }
        | Operation::Hsl { .. }
        // ラスタライズされた SVG は sRGB 符号値の RGBA。合成式もレイヤーと同じ
        // (DESIGN.md §9.7 / §9.9)。
        | Operation::SvgOverlay { .. }
        | Operation::Grain { .. }
        | Operation::GradientMap { .. }
        | Operation::AutoLevels { .. } => Some(Space::Srgb),
    }
}

/// 作業空間の遅延切り替え。すでに目的の空間なら何もしない
/// (= 同じ空間の op が連続するときは変換を往復させない)。
///
/// これは性能のためだけでなく **精度のため**でもある: 変換 1 回あたり
/// 符号値で 2e-5 程度の補間誤差が乗るので、op ごとに往復させると
/// トーンスタックを重ねたときに誤差が積み上がる。
fn ensure_space(img: &mut LinearImage, current: &mut Space, want: Space) {
    if *current == want {
        return;
    }
    match want {
        Space::Srgb => img.encode_in_place(),
        Space::Linear => img.decode_in_place(),
    }
    *current = want;
}

fn op_name(op: &Operation) -> &'static str {
    match op {
        Operation::AutoOrient => "auto_orient",
        Operation::Rotate { .. } => "rotate",
        Operation::Crop { .. } => "crop",
        Operation::Resize { .. } => "resize",
        Operation::Adjust { .. } => "adjust",
        Operation::Encode { .. } => "encode",
        Operation::Perspective { .. } => "perspective",
        Operation::ColorMatrix { .. } => "color_matrix",
        Operation::Curves { .. } => "curves",
        Operation::Levels { .. } => "levels",
        Operation::Blur { .. } => "blur",
        Operation::Median { .. } => "median",
        Operation::UnsharpMask { .. } => "unsharp_mask",
        Operation::Lut { .. } => "lut",
        Operation::WhiteBalance { .. } => "white_balance",
        Operation::Hsl { .. } => "hsl",
        Operation::Convolve { .. } => "convolve",
        Operation::Clone { .. } => "clone",
        Operation::Heal { .. } => "heal",
        Operation::SvgOverlay { .. } => "svg_overlay",
        Operation::Flip { .. } => "flip",
        Operation::Vignette { .. } => "vignette",
        Operation::Grain { .. } => "grain",
        Operation::GradientMap { .. } => "gradient_map",
        Operation::Pixelate { .. } => "pixelate",
        Operation::AutoLevels { .. } => "auto_levels",
        Operation::StripMetadata { .. } => "strip_metadata",
    }
}

/// 入力フォーマットのうち、v1 でエンコードもできるもの。
fn output_format_of(format: ImageFormat) -> Option<OutputFormat> {
    match format {
        ImageFormat::Jpeg => Some(OutputFormat::Jpeg),
        ImageFormat::Png => Some(OutputFormat::Png),
        ImageFormat::WebP => Some(OutputFormat::Webp),
        ImageFormat::Avif => Some(OutputFormat::Avif),
        _ => None,
    }
}

fn check_byte_limit(bytes: &[u8], limits: &Limits) -> Result<()> {
    let len = bytes.len() as u64;
    if len > limits.max_bytes {
        return Err(AtxError::LimitExceeded(format!(
            "input is {len} bytes, limit is {} bytes",
            limits.max_bytes
        )));
    }
    if bytes.is_empty() {
        return Err(AtxError::Decode("input is empty".into()));
    }
    Ok(())
}

fn check_pixel_limit(width: u32, height: u32, limits: &Limits) -> Result<()> {
    let pixels = width as u64 * height as u64;
    if pixels > limits.max_pixels {
        return Err(AtxError::LimitExceeded(format!(
            "input is {width}x{height} = {pixels} pixels, limit is {} pixels",
            limits.max_pixels
        )));
    }
    Ok(())
}

/// マジックバイトでフォーマットを判定し、`image` の limits を適用したリーダを作る。
fn build_reader<'a>(bytes: &'a [u8], limits: &Limits) -> Result<ImageReader<Cursor<&'a [u8]>>> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| AtxError::Decode(e.to_string()))?;
    if reader.format().is_none() {
        return Err(AtxError::Decode(
            "unrecognized image format (magic bytes did not match any supported format)".into(),
        ));
    }
    let mut image_limits = image::Limits::default();
    // 幅・高さ単体の上限は設けず、画素数(幅×高さ)はヘッダから得た寸法を
    // `check_pixel_limit` で検査する(フルデコード前に弾けるのでデコード爆弾対策になる)。
    // ここでは RGBA8 の出力バッファ + 作業領域を見込んだアロケーション上限だけを渡す。
    image_limits.max_alloc = Some(limits.max_pixels.saturating_mul(8));
    reader.limits(image_limits);
    Ok(reader)
}

/// kamadak-exif で EXIF を読む。EXIF が無い/壊れている場合は空の結果を返す。
fn read_exif(bytes: &[u8]) -> ExifInfo {
    let mut info = ExifInfo {
        orientation: None,
        has_gps: false,
        has_any: false,
        summary: BTreeMap::new(),
    };
    let mut cursor = Cursor::new(bytes);
    let Ok(exif) = exif::Reader::new().read_from_container(&mut cursor) else {
        return info;
    };
    info.has_any = exif.fields().next().is_some();

    for field in exif.fields() {
        if field.tag.context() == exif::Context::Gps {
            info.has_gps = true;
        }
    }
    if let Some(field) = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY) {
        info.orientation = field
            .value
            .get_uint(0)
            .filter(|v| (1..=8).contains(v))
            .map(|v| v as u16);
    }

    const SUMMARY_TAGS: &[(&str, exif::Tag)] = &[
        ("camera_make", exif::Tag::Make),
        ("camera_model", exif::Tag::Model),
        ("lens_model", exif::Tag::LensModel),
        ("datetime_original", exif::Tag::DateTimeOriginal),
        ("datetime", exif::Tag::DateTime),
        ("exposure_time", exif::Tag::ExposureTime),
        ("f_number", exif::Tag::FNumber),
        ("iso", exif::Tag::PhotographicSensitivity),
        ("focal_length", exif::Tag::FocalLength),
        ("software", exif::Tag::Software),
    ];
    for (key, tag) in SUMMARY_TAGS {
        if let Some(field) = exif
            .get_field(*tag, exif::In::PRIMARY)
            .or_else(|| exif.get_field(*tag, exif::In::THUMBNAIL))
        {
            let value = field.display_value().with_unit(&exif).to_string();
            let value = value.trim().trim_matches('"').trim().to_string();
            if !value.is_empty() {
                info.summary.insert((*key).to_string(), value);
            }
        }
    }
    if let Some(o) = info.orientation {
        info.summary
            .insert("orientation".to_string(), o.to_string());
    }
    info
}
