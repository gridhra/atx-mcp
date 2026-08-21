//! gradient_map(グラデーションマップ / デュオトーン)。
//!
//! sRGB f32 空間: 画素の BT.709 輝度(sRGB 符号値ベース)を 0..1 に取り、
//! stops(position 昇順)の線形補間色へ写像する。端の外側は端の色でクランプ。
//! 色パースは recipe::parse_hex_color を再利用。アルファ不変。

use crate::linear::LinearImage;
use crate::parallel;
use crate::recipe::{parse_hex_color, GradientStop};
use crate::{AtxError::InvalidRecipe, Result};

/// stops の数の許容範囲。
const MIN_STOPS: usize = 2;
const MAX_STOPS: usize = 8;

/// BT.709 輝度係数(sRGB 符号値ベース)。
const LUMA_R: f64 = 0.2126;
const LUMA_G: f64 = 0.7152;
const LUMA_B: f64 = 0.0722;

/// stops の静的検証。
///
/// - 個数は 2..=8
/// - `position` は有限かつ 0.0..=1.0、狭義単調増加(重複・逆転はエラー)
/// - `color` は `recipe::parse_hex_color` でパース可能な CSS hex
///
/// # hex のアルファについて
///
/// `parse_hex_color` は `#rrggbbaa`(8桁)も受理するが、gradient_map の停止点色は
/// 「不透明な色」を指す語彙(グラデーションマップは輝度→色の写像であり、
/// 画素のアルファは別途「アルファ不変」で温存される)。アルファ入りの hex を
/// 静かに無視すると「指定したのに効かない」という驚きに繋がるため、**8桁 hex は
/// この op では拒否する**(3/4/6 桁のみ許可。4桁はアルファ無しの `#rgb` と等価な
/// 短縮記法なので許可、実質アルファは常に不透明として扱う一貫性を優先した)。
pub fn validate_stops(index: usize, stops: &[GradientStop]) -> Result<()> {
    let fail =
        |message: String| InvalidRecipe(format!("operations[{index}] (gradient_map): {message}"));

    if stops.len() < MIN_STOPS || stops.len() > MAX_STOPS {
        return Err(fail(format!(
            "stops must have {MIN_STOPS}..={MAX_STOPS} entries, got {}",
            stops.len()
        )));
    }

    let mut prev: Option<f64> = None;
    for (i, stop) in stops.iter().enumerate() {
        if !stop.position.is_finite() || !(0.0..=1.0).contains(&stop.position) {
            return Err(fail(format!(
                "stops[{i}].position must be finite and within 0.0..=1.0, got {}",
                stop.position
            )));
        }
        if let Some(p) = prev {
            if stop.position <= p {
                return Err(fail(format!(
                    "stops[{i}].position must be strictly increasing, got {} after {}",
                    stop.position, p
                )));
            }
        }
        prev = Some(stop.position);

        if stop.color.trim_start_matches('#').len() == 8 {
            return Err(fail(format!(
                "stops[{i}].color: 8-digit hex (#rrggbbaa) is not accepted here \
                 (gradient_map stops are opaque colors; pixel alpha is preserved \
                 unchanged separately), got {:?}",
                stop.color
            )));
        }
        if parse_hex_color(&stop.color).is_none() {
            return Err(fail(format!(
                "stops[{i}].color must be a CSS hex color (#rgb / #rrggbb), got {:?}",
                stop.color
            )));
        }
    }

    Ok(())
}

/// stops を (position: f64, rgb: [f64; 3] in 0..1) へ前処理する。
fn prepare_stops(stops: &[GradientStop]) -> Vec<(f64, [f64; 3])> {
    stops
        .iter()
        .map(|s| {
            let rgba = parse_hex_color(&s.color).unwrap_or([0, 0, 0, 255]);
            let rgb = [
                rgba[0] as f64 / 255.0,
                rgba[1] as f64 / 255.0,
                rgba[2] as f64 / 255.0,
            ];
            (s.position, rgb)
        })
        .collect()
}

/// 輝度 `luma`(0..1)に対応する色を stops から線形補間で求める(f64 演算)。
/// 端の外側は端の色でクランプする。
fn color_at(prepared: &[(f64, [f64; 3])], luma: f64) -> [f32; 3] {
    let first = prepared.first().expect("validate guarantees >= 2 stops");
    let last = prepared.last().expect("validate guarantees >= 2 stops");

    if luma <= first.0 {
        return [first.1[0] as f32, first.1[1] as f32, first.1[2] as f32];
    }
    if luma >= last.0 {
        return [last.1[0] as f32, last.1[1] as f32, last.1[2] as f32];
    }

    // 区間探索: luma を含む区間 [prepared[i], prepared[i+1]) を線形走査で見つける。
    // stops は高々 8 個なので二分探索の必要はない。
    for pair in prepared.windows(2) {
        let (p0, c0) = pair[0];
        let (p1, c1) = pair[1];
        if luma >= p0 && luma <= p1 {
            let span = p1 - p0;
            let t = if span > 0.0 { (luma - p0) / span } else { 0.0 };
            let mut out = [0f32; 3];
            for c in 0..3 {
                let v = c0[c] + t * (c1[c] - c0[c]);
                out[c] = v as f32;
            }
            return out;
        }
    }
    // 到達しないはずだが、安全側として最後の色を返す。
    [last.1[0] as f32, last.1[1] as f32, last.1[2] as f32]
}

/// グラデーションマップを適用する(**sRGB 符号値空間**、エンジン側で空間変換済みの前提)。
///
/// 画素ごとに BT.709 輝度(sRGB 符号値ベース)を求め、その値で stops を
/// 線形補間した色を RGB へ書き込む。アルファは不変。
pub fn apply(img: &LinearImage, stops: &[GradientStop]) -> LinearImage {
    let prepared = prepare_stops(stops);
    if prepared.len() < MIN_STOPS {
        return img.clone();
    }

    let mut out = img.clone();
    parallel::for_each_chunk(&mut out.data, |chunk| {
        for px in chunk.iter_mut() {
            let luma = LUMA_R * px[0] as f64 + LUMA_G * px[1] as f64 + LUMA_B * px[2] as f64;
            let rgb = color_at(&prepared, luma);
            px[0] = rgb[0];
            px[1] = rgb[1];
            px[2] = rgb[2];
            // px[3](アルファ)は不変。
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop(position: f64, color: &str) -> GradientStop {
        GradientStop {
            position,
            color: color.to_string(),
        }
    }

    #[test]
    fn validate_accepts_two_stops() {
        assert!(validate_stops(0, &[stop(0.0, "#000000"), stop(1.0, "#ffffff")]).is_ok());
    }

    #[test]
    fn validate_rejects_single_stop() {
        assert!(validate_stops(0, &[stop(0.0, "#000000")]).is_err());
    }

    #[test]
    fn validate_rejects_non_increasing_positions() {
        let err = validate_stops(0, &[stop(0.5, "#000000"), stop(0.5, "#ffffff")]);
        assert!(err.is_err());
    }

    #[test]
    fn validate_rejects_too_many_stops() {
        let stops: Vec<GradientStop> = (0..9).map(|i| stop(i as f64 / 8.0, "#ffffff")).collect();
        assert!(validate_stops(0, &stops).is_err());
    }

    #[test]
    fn validate_rejects_bad_hex() {
        assert!(validate_stops(0, &[stop(0.0, "notacolor"), stop(1.0, "#ffffff")]).is_err());
    }

    #[test]
    fn validate_rejects_eight_digit_hex() {
        assert!(validate_stops(0, &[stop(0.0, "#00000080"), stop(1.0, "#ffffff")]).is_err());
    }
}
