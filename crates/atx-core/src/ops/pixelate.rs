//! pixelate(モザイク)。プライバシー用途(顔・番号標)の定番。
//!
//! ブロック平均は**リニア光**・**プリマルチプライアルファ**で行う(平均 = 光の合成)。
//! region 省略時は全面。
//! region はブロック格子の原点を region 左上に合わせる(部分モザイクの見た目安定)。
//! 端の半端ブロックは実画素数で平均。決定論: 固定順序 f64 累算。

use crate::linear::{LinearImage, ALPHA_EPSILON};
use crate::recipe::Rect;
use crate::{AtxError::InvalidRecipe, Result};

const MIN_BLOCK: u32 = 2;
const MAX_BLOCK: u32 = 256;

pub fn validate(index: usize, block_size: u32, region: &Option<Rect>) -> Result<()> {
    let fail =
        |message: String| InvalidRecipe(format!("operations[{index}] (pixelate): {message}"));

    if !(MIN_BLOCK..=MAX_BLOCK).contains(&block_size) {
        return Err(fail(format!(
            "block_size must be within {MIN_BLOCK}..={MAX_BLOCK}, got {block_size}"
        )));
    }
    if let Some(r) = region {
        if r.width == 0 || r.height == 0 {
            return Err(fail(format!(
                "region width/height must be > 0, got {}x{}",
                r.width, r.height
            )));
        }
    }
    Ok(())
}

/// region(省略時は全面)を画像範囲へクランプする。交差が空なら None。
fn clamp_region(region: Option<Rect>, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
    let r = region.unwrap_or(Rect {
        x: 0,
        y: 0,
        width: w,
        height: h,
    });
    let x0 = r.x.min(w);
    let y0 = r.y.min(h);
    let x1 = r.x.saturating_add(r.width).min(w);
    let y1 = r.y.saturating_add(r.height).min(h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((x0, y0, x1, y1))
}

/// プリマルチプライ空間の合計をストレートアルファの平均画素へ戻す。
///
/// 規則は `linear::LinearImage::unpremultiply` と同一(`a < ALPHA_EPSILON` は
/// RGB を 0、それ以外は `/a` して 0..1 クランプ)。
fn unpremultiplied_mean(sum: [f64; 4], count: f64) -> [f32; 4] {
    if count <= 0.0 {
        return [0.0, 0.0, 0.0, 0.0];
    }
    let a = ((sum[3] / count) as f32).clamp(0.0, 1.0);
    if a < ALPHA_EPSILON {
        return [0.0, 0.0, 0.0, a];
    }
    let mut out = [0f32; 4];
    for (c, slot) in out.iter_mut().enumerate().take(3) {
        let mean = (sum[c] / count) as f32;
        *slot = (mean / a).clamp(0.0, 1.0);
    }
    out[3] = a;
    out
}

/// モザイクを適用する(**線形光空間**、エンジン側で空間変換済みの前提)。
///
/// region の外は完全に不変(byte-identical)。region 内はブロック平均(RGBA 4ch、
/// f64 固定順序累算)で塗り潰す。格子は region の左上(クランプ後)を原点に置く。
///
/// 平均は **アルファをプリマルチプライしてから**取り、最後に解く
/// (`ops/mod.rs` の「画素を混ぜる op」の規約。blur / resize と同じ)。
/// ストレートアルファのまま平均すると、透明な画素の RGB(=「色を持たない」はずの値)が
/// 不透明な画素の色へ混ざり込む — 不透明な赤 + 透明な緑のブロックが黄色くなる、
/// という形で目に見える。全画素 α=1.0 の画像ではプリマルチプライは厳密な恒等なので、
/// 不透明画像の出力はビット同一のまま。
///
/// 失敗は呼び出し側(engine)が `AtxError::Operation` へ包む前提の平文メッセージで返す
/// (`clone_heal` と同じ規約)。
pub fn apply(
    img: &LinearImage,
    block_size: u32,
    region: &Option<Rect>,
) -> std::result::Result<LinearImage, String> {
    let (w, h) = img.dimensions();

    if w == 0 || h == 0 {
        return Ok(img.clone());
    }

    let Some((x0, y0, x1, y1)) = clamp_region(*region, w, h) else {
        let r = region.expect("None region always clamps to the full image");
        return Err(format!(
            "pixelate: region {}x{}+{}+{} does not intersect the {w}x{h} image",
            r.width, r.height, r.x, r.y
        ));
    };

    let block_size = block_size.max(1);
    let premul = img.premultiplied();
    let mut out = img.clone();

    let mut by = y0;
    while by < y1 {
        let by_end = (by + block_size).min(y1);
        let mut bx = x0;
        while bx < x1 {
            let bx_end = (bx + block_size).min(x1);

            // f64 固定順序累算(走査順: 上から下、各行は左から右)。
            let mut sum = [0f64; 4];
            let mut count = 0f64;
            for y in by..by_end {
                for x in bx..bx_end {
                    let px = premul.get(x, y);
                    for c in 0..4 {
                        sum[c] += px[c] as f64;
                    }
                    count += 1.0;
                }
            }
            let mean = unpremultiplied_mean(sum, count);

            for y in by..by_end {
                for x in bx..bx_end {
                    out.set(x, y, mean);
                }
            }

            bx = bx_end;
        }
        by = by_end;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_default_block_size() {
        assert!(validate(0, 8, &None).is_ok());
    }

    #[test]
    fn validate_rejects_block_size_below_min() {
        assert!(validate(0, 1, &None).is_err());
    }

    #[test]
    fn validate_rejects_block_size_above_max() {
        assert!(validate(0, 257, &None).is_err());
    }

    #[test]
    fn validate_rejects_zero_size_region() {
        let region = Some(Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 4,
        });
        assert!(validate(0, 4, &region).is_err());
    }

    /// region が画像から外れたときは**平文メッセージ**で返す(engine が
    /// `AtxError::Operation` へ 1 度だけ包む。`clone_heal` と同じ規約)。
    /// 以前は自前で `InvalidRecipe` を作っていたため、engine 側の包み直しと重なって
    /// "invalid recipe:" が二重に付いたメッセージになっていた。
    #[test]
    fn apply_rejects_region_outside_image() {
        let img = LinearImage::from_pixel(4, 4, [0.5, 0.5, 0.5, 1.0]);
        let region = Some(Rect {
            x: 100,
            y: 100,
            width: 4,
            height: 4,
        });
        let err = apply(&img, 2, &region).unwrap_err();
        assert!(err.starts_with("pixelate: region"), "{err}");
        assert!(!err.contains("invalid recipe"), "{err}");
    }

    /// 回帰: 透明画素の RGB が不透明画素へ滲まない(プリマルチプライ平均)。
    ///
    /// 不透明な赤 (1,0,0,1) と透明な緑 (0,1,0,0) の 2 画素ブロック。
    /// ストレートアルファのまま平均すると RGB が (0.5, 0.5, 0) = 黄色になっていた。
    /// プリマルチプライして平均すると (0.5, 0, 0, 0.5) → 解いて (1, 0, 0) at α=0.5、
    /// つまり「色は赤のまま、半透明になる」という blur と一貫した結果になる。
    #[test]
    fn transparent_pixels_do_not_bleed_color() {
        let mut img = LinearImage::new(2, 1);
        img.set(0, 0, [1.0, 0.0, 0.0, 1.0]);
        img.set(1, 0, [0.0, 1.0, 0.0, 0.0]);
        let out = apply(&img, 2, &None).unwrap();
        assert_eq!(out.get(0, 0), [1.0, 0.0, 0.0, 0.5]);
        assert_eq!(out.get(1, 0), [1.0, 0.0, 0.0, 0.5]);
    }

    /// 完全に透明なブロックは RGB を 0 にする(`unpremultiply` と同じ規則)。
    #[test]
    fn fully_transparent_block_zeroes_rgb() {
        let img = LinearImage::from_pixel(2, 1, [0.3, 0.7, 0.1, 0.0]);
        let out = apply(&img, 2, &None).unwrap();
        assert_eq!(out.get(0, 0), [0.0, 0.0, 0.0, 0.0]);
    }

    /// 全画素が不透明なら、プリマルチプライは厳密な恒等 = 従来と同じ平均。
    #[test]
    fn opaque_block_average_is_unchanged() {
        let mut img = LinearImage::new(2, 1);
        img.set(0, 0, [1.0, 1.0, 1.0, 1.0]);
        img.set(1, 0, [0.0, 0.0, 0.0, 1.0]);
        let out = apply(&img, 2, &None).unwrap();
        assert_eq!(out.get(0, 0), [0.5, 0.5, 0.5, 1.0]);
    }
}
