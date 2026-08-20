//! 決定論的変換エンジン: bytes in → bytes out。
//! 同一入力バイト列 + 同一レシピ → バイト同一出力(ゴールデンテストで回帰検証する)。

use std::collections::BTreeMap;
use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbaImage};
use serde::Serialize;

use crate::codec;
use crate::pixel_ops;
use crate::recipe::{
    parse_aspect_ratio, parse_hex_color, CoordinateSpace, Operation, OutputFormat, StripScope,
    TransformRecipe,
};
use crate::transform::{map_source_rect, Affine};
use crate::{AtxError, Limits, Result};

/// エンジンの挙動バージョン。出力バイト列に影響する変更を入れたら上げる。
/// ゴールデンテストはこのバージョンの挙動をピン留めしている。
pub const ENGINE_VERSION: &str = "atx-core/1";

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
/// (orientation 正規化 / rotate / crop / pad / resize。adjust・encode・strip は座標を動かさない)。
/// 詳細は `crate::transform` と `recipe::CoordinateSpace` を参照。
pub fn apply_recipe(
    bytes: &[u8],
    recipe: &TransformRecipe,
    limits: &Limits,
) -> Result<EncodedOutput> {
    crate::recipe::validate(recipe)?;
    check_byte_limit(bytes, limits)?;

    let mut warnings = Vec::new();

    // --- デコード ---
    let reader = build_reader(bytes, limits)?;
    let input_format = reader.format();
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| AtxError::Decode(e.to_string()))?;
    let (width, height) = decoder.dimensions();
    check_pixel_limit(width, height, limits)?;
    let mut has_alpha = decoder.color_type().has_alpha();
    let mut icc = decoder
        .icc_profile()
        .ok()
        .flatten()
        .filter(|p| !p.is_empty());
    let decoded =
        DynamicImage::from_decoder(decoder).map_err(|e| AtxError::Decode(e.to_string()))?;
    let mut img: RgbaImage = decoded.to_rgba8();

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

    // --- op を順次適用 ---
    let mut strip: Option<StripScope> = None;
    for (index, op) in recipe.operations.iter().enumerate() {
        let fail = |message: String| AtxError::Operation {
            index,
            op: op_name(op).to_string(),
            message,
        };
        match op {
            // Orientation はデコード直後に正規化済みのため、ここでは何もしない。
            Operation::AutoOrient => {}
            Operation::Rotate {
                angle_degrees,
                crop,
            } => {
                let (rotated, warning, step) =
                    pixel_ops::rotate(&img, *angle_degrees, *crop, DEFAULT_PAD);
                img = rotated;
                xf = xf.then(step);
                if let Some(w) = warning {
                    warnings.push(w);
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
                let pad = match pad_color {
                    Some(c) => parse_hex_color(c)
                        .ok_or_else(|| fail(format!("invalid pad_color {c:?}")))?,
                    None => DEFAULT_PAD,
                };
                if let Some(ratio) = aspect_ratio {
                    let ratio = parse_aspect_ratio(ratio)
                        .ok_or_else(|| fail(format!("invalid aspect_ratio {ratio:?}")))?;
                    let (fitted, step) = pixel_ops::fit_aspect(&img, ratio, *anchor, *mode, pad);
                    img = fitted;
                    xf = xf.then(step);
                    if *mode == crate::recipe::CropMode::Pad && pad[3] < 255 {
                        has_alpha = true;
                    }
                } else if let Some(rect) = rect {
                    // source 座標指定なら、ここまでの幾何変換で矩形を現在の座標系へ写す。
                    let effective = match coordinate_space {
                        CoordinateSpace::Current => *rect,
                        CoordinateSpace::Source => {
                            let (cw, ch) = img.dimensions();
                            let mapped = map_source_rect(&xf, *rect, cw, ch).map_err(fail)?;
                            if mapped.clamped {
                                warnings.push(format!(
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
                    img = pixel_ops::crop_rect(&img, effective).map_err(fail)?;
                    xf = xf.then(Affine::translate(
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
                let (iw, ih) = img.dimensions();
                let ((sw, sh), (cw, ch)) =
                    pixel_ops::resize_targets(iw, ih, *width, *height, *fit, *without_enlargement);
                img = pixel_ops::resize_lanczos3(&img, sw, sh, has_alpha).map_err(fail)?;
                // 連続座標では拡縮は原点固定の純粋なスケール。
                xf = xf.then(Affine::scale(sw as f64 / iw as f64, sh as f64 / ih as f64));
                if (cw, ch) != (sw, sh) {
                    let x = (sw - cw) / 2;
                    let y = (sh - ch) / 2;
                    img = image::imageops::crop_imm(&img, x, y, cw, ch).to_image();
                    // fit=cover の内部中央クロップぶんの平行移動。
                    xf = xf.then(Affine::translate(-(x as f64), -(y as f64)));
                }
            }
            Operation::Adjust {
                brightness,
                contrast,
                saturation,
                sharpness,
            } => {
                img = pixel_ops::adjust(&img, *brightness, *contrast, *saturation, *sharpness);
            }
            Operation::StripMetadata { scope } => {
                strip = Some(*scope);
            }
            // エンコード指定は最後にまとめて処理する(validate により最後の op であることが保証される)。
            Operation::Encode { .. } => {}
        }
    }

    // --- 出力フォーマットの決定 ---
    let encode_op = recipe.operations.iter().find_map(|op| match op {
        Operation::Encode { format, quality } => Some((*format, *quality)),
        _ => None,
    });
    let (format, quality) = match encode_op {
        Some(v) => v,
        None => match input_format.and_then(output_format_of) {
            Some(f) => (f, None),
            None => {
                warnings.push(format!(
                    "input format {:?} cannot be re-encoded; falling back to jpeg",
                    input_format
                ));
                (OutputFormat::Jpeg, None)
            }
        },
    };

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
    if exif.has_any {
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
    let (out_bytes, icc_embedded) =
        codec::encode(&img, format, quality, out_has_alpha, icc.as_deref())?;
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

fn op_name(op: &Operation) -> &'static str {
    match op {
        Operation::AutoOrient => "auto_orient",
        Operation::Rotate { .. } => "rotate",
        Operation::Crop { .. } => "crop",
        Operation::Resize { .. } => "resize",
        Operation::Adjust { .. } => "adjust",
        Operation::Encode { .. } => "encode",
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
