//! 出力エンコーダ。すべて固定設定で呼び出し、同一画素 → 同一バイト列を保証する。
//!
//! エンコーダのバージョンは Cargo.lock で固定される前提(DESIGN §5「決定論」)。

use image::{ExtendedColorType, ImageEncoder, RgbImage, RgbaImage};

use crate::recipe::OutputFormat;
use crate::{AtxError, Result};

/// フォーマット別のデフォルト品質。
pub(crate) const DEFAULT_JPEG_QUALITY: u8 = 85;
pub(crate) const DEFAULT_WEBP_QUALITY: u8 = 82;
pub(crate) const DEFAULT_AVIF_QUALITY: u8 = 80;
/// AVIF の速度。決定論のため定数固定(値を変えると出力バイト列が変わる)。
pub(crate) const AVIF_SPEED: u8 = 6;

/// 出力フォーマットの MIME type。
pub(crate) fn mime_type(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Jpeg => "image/jpeg",
        OutputFormat::Png => "image/png",
        OutputFormat::Webp => "image/webp",
        OutputFormat::Avif => "image/avif",
    }
}

/// RGBA を不透明背景(白)に合成して RGB にする。JPEG のようにアルファを持てない出力用。
fn flatten_to_rgb(img: &RgbaImage) -> RgbImage {
    let (w, h) = img.dimensions();
    let mut out = RgbImage::new(w, h);
    for (dst, src) in out.pixels_mut().zip(img.pixels()) {
        let a = src.0[3] as u32;
        for i in 0..3 {
            // 白背景 (255) との alpha 合成。a=255 のときは元の値と厳密に一致する。
            let v = (src.0[i] as u32 * a + 255 * (255 - a) + 127) / 255;
            dst.0[i] = v.min(255) as u8;
        }
    }
    out
}

/// 指定フォーマットでエンコードする。
///
/// - `has_alpha=false` の場合、アルファチャンネルを持たない表現(RGB8)で書き出す
/// - `icc` が Some かつフォーマットが対応する場合のみ ICC プロファイルを埋め込む
///   (v1 では JPEG のみ埋め込みに対応。他フォーマットでは破棄され warning が出る)
pub(crate) fn encode(
    img: &RgbaImage,
    format: OutputFormat,
    quality: Option<u8>,
    has_alpha: bool,
    icc: Option<&[u8]>,
) -> Result<(Vec<u8>, bool)> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Err(AtxError::Encode("image has zero dimension".into()));
    }
    match format {
        OutputFormat::Jpeg => encode_jpeg(img, quality.unwrap_or(DEFAULT_JPEG_QUALITY), icc),
        OutputFormat::Png => encode_png(img, has_alpha).map(|b| (b, false)),
        OutputFormat::Webp => {
            encode_webp(img, quality.unwrap_or(DEFAULT_WEBP_QUALITY), has_alpha).map(|b| (b, false))
        }
        OutputFormat::Avif => {
            encode_avif(img, quality.unwrap_or(DEFAULT_AVIF_QUALITY), has_alpha).map(|b| (b, false))
        }
    }
}

/// JPEG(`jpeg-encoder`)。ICC プロファイルは APP2 セグメントとして埋め込める。
/// 戻り値の bool は ICC を実際に埋め込んだかどうか。
///
/// # 出力は必ず「単一インターリーブスキャンのベースライン JPEG」にする
///
/// `jpeg-encoder` は `set_optimized_huffman_tables(true)` を指定すると、
/// 最適化テーブルを作るために **非インターリーブ(コンポーネントごとに SOS を分けた
/// 3 スキャン)** のベースライン JPEG を書き出す(`encoder.rs` の
/// `optimize_huffman_table || !supports_interleaved()` 分岐)。
///
/// この形式は仕様上は妥当(libjpeg / macOS は正しく読める)だが、
/// **atx-core 自身が入力デコードに使う `image` 0.25 の JPEG デコーダ
/// (zune-jpeg 0.5)が、クロマサブサンプリングを伴う非インターリーブ多重スキャンを
/// 正しく復号できない**。Cb/Cr スキャンが丸ごと 0 のまま残り、
/// Y だけが復元されて「緑一色 + 横縞」の画像になる。
/// jpeg-encoder は quality < 90 で自動的に 4:2:0 を選ぶため、
/// 既定品質(85)で出力した JPEG revision は、それを入力として再変換した瞬間に破壊されていた。
///
/// したがって最適化ハフマンテーブルは使わない。
/// ファイルサイズは数 % 増えるが、画質は同一で、出力は
/// 「単一インターリーブスキャン」という最も互換性の高い形になる。
fn encode_jpeg(img: &RgbaImage, quality: u8, icc: Option<&[u8]>) -> Result<(Vec<u8>, bool)> {
    let (w, h) = img.dimensions();
    if w > u16::MAX as u32 || h > u16::MAX as u32 {
        return Err(AtxError::Encode(format!(
            "jpeg does not support dimensions larger than 65535 ({w}x{h})"
        )));
    }
    let rgb = flatten_to_rgb(img);
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder.set_progressive(false);
    // 非インターリーブ多重スキャンを誘発するため有効化しない(上のドキュメント参照)。
    encoder.set_optimized_huffman_tables(false);
    let mut icc_embedded = false;
    if let Some(profile) = icc {
        if !profile.is_empty() && encoder.add_icc_profile(profile).is_ok() {
            icc_embedded = true;
        }
    }
    encoder
        .encode(
            rgb.as_raw(),
            w as u16,
            h as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .map_err(|e| AtxError::Encode(format!("jpeg: {e}")))?;
    Ok((out, icc_embedded))
}

/// PNG(`image` の PngEncoder、既定圧縮・フィルタ)。可逆なので quality は無視する。
fn encode_png(img: &RgbaImage, has_alpha: bool) -> Result<Vec<u8>> {
    let (w, h) = img.dimensions();
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    if has_alpha {
        encoder
            .write_image(img.as_raw(), w, h, ExtendedColorType::Rgba8)
            .map_err(|e| AtxError::Encode(format!("png: {e}")))?;
    } else {
        let rgb = flatten_to_rgb(img);
        encoder
            .write_image(rgb.as_raw(), w, h, ExtendedColorType::Rgb8)
            .map_err(|e| AtxError::Encode(format!("png: {e}")))?;
    }
    Ok(out)
}

/// WebP(libwebp、lossy 固定)。純 Rust の lossy エンコーダが存在しないため FFI を使う。
fn encode_webp(img: &RgbaImage, quality: u8, has_alpha: bool) -> Result<Vec<u8>> {
    let (w, h) = img.dimensions();
    let memory = if has_alpha {
        webp::Encoder::from_rgba(img.as_raw(), w, h).encode(quality as f32)
    } else {
        let rgb = flatten_to_rgb(img);
        webp::Encoder::from_rgb(rgb.as_raw(), w, h).encode(quality as f32)
    };
    Ok(memory.to_vec())
}

/// AVIF(`ravif` / rav1e)。速度は定数固定、スレッド数は 1 に固定して決定論を担保する。
fn encode_avif(img: &RgbaImage, quality: u8, has_alpha: bool) -> Result<Vec<u8>> {
    use ravif::{Encoder, Img, RGB8, RGBA8};
    let (w, h) = img.dimensions();
    let encoder = Encoder::new()
        .with_quality(quality as f32)
        .with_alpha_quality(quality as f32)
        .with_speed(AVIF_SPEED)
        // rav1e はタイル分割・スレッド数で出力が変わりうるため 1 に固定する。
        .with_num_threads(Some(1));
    let encoded = if has_alpha {
        let pixels: Vec<RGBA8> = img
            .pixels()
            .map(|p| RGBA8::new(p.0[0], p.0[1], p.0[2], p.0[3]))
            .collect();
        encoder.encode_rgba(Img::new(pixels.as_slice(), w as usize, h as usize))
    } else {
        let rgb = flatten_to_rgb(img);
        let pixels: Vec<RGB8> = rgb
            .pixels()
            .map(|p| RGB8::new(p.0[0], p.0[1], p.0[2]))
            .collect();
        encoder.encode_rgb(Img::new(pixels.as_slice(), w as usize, h as usize))
    };
    encoded
        .map(|e| e.avif_file)
        .map_err(|e| AtxError::Encode(format!("avif: {e}")))
}
