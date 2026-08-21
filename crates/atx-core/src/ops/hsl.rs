//! 色相域別 HSL 調整(8域: red/orange/yellow/green/aqua/blue/purple/magenta)。
//!
//! - RGB → HSL 変換は f64 固定式(超越関数不使用の区分有理式で書けるため libm 遮断は容易)
//! - 各域は中心色相で重み1、隣接域中心へ線形に減衰する三角フェザ
//! - hue シフトは度数に写像(±100 → ±30°)、sat/lum は乗算的スケール
//! - 決定論: 固定順序の四則演算のみ。域重みテーブルの係数は定数で明文化
//!
//! # 色相域の中心(度)
//!
//! ```text
//! red = 0, orange = 30, yellow = 60, green = 120,
//! aqua = 180, blue = 240, purple = 280, magenta = 320
//! ```
//!
//! 間隔が不均等(30/30/60/60/60/40/40/40)なのは意図的で、Lightroom の HSL パネルの
//! 知覚的な帯割りに合わせている(黄〜赤の暖色側は色相の変化が知覚的に速いため帯が狭く、
//! 緑〜青の寒色側は広い)。この 8 定数は `recipe.rs` の `Operation::Hsl` の
//! フィールド宣言順と一致する。
//!
//! # 帯の重み(三角フェザ)
//!
//! 円周上で隣り合う中心 `c_i <= h < c_{i+1}` を見つけ、
//! `t = (h - c_i) / (c_{i+1} - c_i)` として
//! `w_i = 1 - t`, `w_{i+1} = t`、他の帯は 0 とする。
//! 中心では重み 1、隣接中心で 0 まで線形に落ち、**常に 2 帯の重み和が厳密に 1**
//! になる(区分線形の分割の統一)。最後の区間は magenta(320) → red(360 ≡ 0) で環を閉じる。
//!
//! # シフトの写像(v0.3 で確定。変更は ENGINE_VERSION 更新を伴う)
//!
//! 画素の色相 `h` に対して重み `w_i` を求め、有効な帯(高々 2 つ)の指定値から:
//!
//! ```text
//! dh     = Σ w_i * hue_i * 0.3                       // ±100 → ±30 度
//! f_sat  = clamp(Σ w_i * saturation_i / 100, -0.95, 1.0)
//! f_lum  = clamp(Σ w_i * luminance_i / 100 * 0.5, -0.95, 1.0)
//!
//! h' = (h + dh) mod 360
//! s' = clamp(s * (1 + f_sat), 0, 1)
//! l' = clamp(l * (1 + f_lum), 0, 1)
//! ```
//!
//! クランプ幅 `-0.95..=1.0` は「下限は完全な 0 倍を避けて単調性を残す(-100 でも
//! 元の 5% は残る)/ 上限は 2 倍まで」という単純で固定の規則。乗算的なので
//! `saturation = 0` や `luminance = 0` は厳密な恒等になる。
//! `luminance` に 0.5 が掛かっているのは、明度の乗算は彩度より知覚影響が大きく、
//! ±100 でも ±50% に留めたいため。
//!
//! # 無彩色画素の扱い
//!
//! `s == 0`(= R == G == B)の画素は色相が定義できないため **一切変更しない**。
//! 灰軸に色を乗せたい要求は将来 split-toning / color-grading の語彙で扱う(Phase C 以降)。
//! この規則により、灰色のみの画像は任意の HSL 指定に対してバイト一致で不変。
//!
//! # 丸めと決定論
//!
//! RGB ⇄ HSL は f64、最終書き戻しは `clamp(0,255)` → half-away-from-zero 丸め。
//! `rgb_to_hsl` → `hsl_to_rgb` の往復は **全 u8 RGB 値でバイト一致**
//! (tests/wb_hsl_ops.rs の網羅テストが品質ゲート)。
//! 全帯が未指定または全値 0 の場合は画像ごと短絡し、画素単位でも寄与 2 帯が
//! ともに 0 なら元画素をそのままコピーする。

use image::RgbaImage;

use crate::recipe::HslShift;
use crate::{AtxError::InvalidRecipe, Result};

/// 帯の中心色相(度)。`recipe.rs` の宣言順と一致させること。
pub const BAND_CENTERS: [f64; 8] = [0.0, 30.0, 60.0, 120.0, 180.0, 240.0, 280.0, 320.0];

/// 帯名(エラーメッセージ用。`BAND_CENTERS` と同順)。
const BAND_NAMES: [&str; 8] = [
    "red", "orange", "yellow", "green", "aqua", "blue", "purple", "magenta",
];

/// スライダの許容範囲(両端含む)。
const SLIDER_MIN: f64 = -100.0;
const SLIDER_MAX: f64 = 100.0;

/// hue スライダ ±100 に対応する色相シフト(度)= 100 * 0.3。
const HUE_DEGREES_PER_UNIT: f64 = 0.3;
/// luminance の乗算率を控えめにする係数(±100 → ±50%)。
const LUMINANCE_SCALE: f64 = 0.5;
/// 乗算係数 `1 + f` の f のクランプ幅。
const FACTOR_MIN: f64 = -0.95;
const FACTOR_MAX: f64 = 1.0;

// ------------------------------------------------------------------ validate

/// 8 域の順序は recipe.rs の宣言順(red, orange, yellow, green, aqua, blue, purple, magenta)。
///
/// - 少なくとも 1 域の指定が必要(全域 None は無意味なのでエラー)
/// - 指定された域の hue / saturation / luminance はすべて有限かつ -100..=100
pub fn validate(index: usize, bands: &[&Option<HslShift>; 8]) -> Result<()> {
    if bands.iter().all(|b| b.is_none()) {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (hsl): at least one of {} is required",
            BAND_NAMES.join(", ")
        )));
    }
    for (band, name) in bands.iter().zip(BAND_NAMES) {
        let Some(shift) = band else { continue };
        for (field, value) in [
            ("hue", shift.hue),
            ("saturation", shift.saturation),
            ("luminance", shift.luminance),
        ] {
            if !value.is_finite() || !(SLIDER_MIN..=SLIDER_MAX).contains(&value) {
                return Err(InvalidRecipe(format!(
                    "operations[{index}] (hsl): {name}.{field} must be a finite value within \
                     {SLIDER_MIN}..={SLIDER_MAX}, got {value}"
                )));
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------- RGB ⇄ HSL 変換

/// RGB(u8) → HSL。無彩色(R == G == B)の場合は色相が定義できないため `None`。
///
/// 返り値は `(h, s, l)`。`h ∈ [0, 360)`、`s ∈ (0, 1]`、`l ∈ (0, 1)`。
/// 標準の区分有理式(超越関数なし)。
pub fn rgb_to_hsl(r: u8, g: u8, b: u8) -> Option<(f64, f64, f64)> {
    let max_u = r.max(g).max(b);
    let min_u = r.min(g).min(b);
    if max_u == min_u {
        return None;
    }
    let rf = r as f64 / 255.0;
    let gf = g as f64 / 255.0;
    let bf = b as f64 / 255.0;
    let max = max_u as f64 / 255.0;
    let min = min_u as f64 / 255.0;

    let l = (max + min) / 2.0;
    let d = max - min;
    let s = if l <= 0.5 {
        d / (max + min)
    } else {
        d / (2.0 - max - min)
    };

    // 6 分割セクタ座標 hp ∈ [0, 6) を先に求め、最後に 60 倍して度へ。
    let hp = if max_u == r {
        let v = (gf - bf) / d;
        if v < 0.0 {
            v + 6.0
        } else {
            v
        }
    } else if max_u == g {
        (bf - rf) / d + 2.0
    } else {
        (rf - gf) / d + 4.0
    };
    Some((hp * 60.0, s, l))
}

/// HSL → RGB(u8)。`rgb_to_hsl` の厳密な逆(全 u8 RGB でバイト一致往復)。
///
/// `h` は任意の実数を受け付け内部で [0, 360) へ正規化する。
/// `s`/`l` は [0, 1] を想定(範囲外は呼び出し側でクランプ済みであること)。
/// 最終値は 0..255 にクランプして half-away-from-zero 丸め。
pub fn hsl_to_rgb(h: f64, s: f64, l: f64) -> [u8; 3] {
    let to_u8 = |v: f64| (v * 255.0).clamp(0.0, 255.0).round() as u8;
    if s <= 0.0 {
        let v = to_u8(l);
        return [v, v, v];
    }
    // [0, 360) へ正規化。
    let mut hh = h % 360.0;
    if hh < 0.0 {
        hh += 360.0;
    }
    if hh >= 360.0 {
        hh = 0.0;
    }
    let hp = hh / 60.0;

    // クロマ c は前方式の s の定義をそのまま反転したもの(c == max - min)。
    let c = if l <= 0.5 {
        2.0 * l * s
    } else {
        (2.0 - 2.0 * l) * s
    };
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let m = l - c / 2.0;

    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [to_u8(r1 + m), to_u8(g1 + m), to_u8(b1 + m)]
}

// -------------------------------------------------------------------- 帯の重み

/// 色相 `h ∈ [0, 360)` に寄与する 2 帯のインデックスと重みを返す。
///
/// 返り値 `((i, w_i), (j, w_j))` で `w_i + w_j == 1`(区分線形の分割の統一)。
fn band_weights(h: f64) -> ((usize, f64), (usize, f64)) {
    // 最後の区間 320 → 360(= red の中心 0 を 360 とみなす)で環を閉じる。
    for i in 0..8 {
        let lo = BAND_CENTERS[i];
        let hi = if i + 1 < 8 {
            BAND_CENTERS[i + 1]
        } else {
            360.0
        };
        if h >= lo && h < hi {
            let t = (h - lo) / (hi - lo);
            let j = if i + 1 < 8 { i + 1 } else { 0 };
            return ((i, 1.0 - t), (j, t));
        }
    }
    // h が [0, 360) にあれば到達しない。安全側で red 100%。
    ((0, 1.0), (1, 0.0))
}

// ------------------------------------------------------------------------ op

/// 色相域別 HSL 調整を適用する。アルファは不変。
///
/// 写像とクランプの規則はモジュールドキュメント参照。全帯が未指定または
/// 全値 0 の場合は入力をそのまま返す(バイト一致保証)。
pub fn apply(img: &RgbaImage, bands: &[Option<HslShift>; 8]) -> RgbaImage {
    // 未指定の帯はゼロシフトへ畳む。以降は 8 個の HslShift として扱う。
    let shifts: [HslShift; 8] = std::array::from_fn(|i| {
        bands[i].unwrap_or(HslShift {
            hue: 0.0,
            saturation: 0.0,
            luminance: 0.0,
        })
    });
    let is_zero = |s: &HslShift| s.hue == 0.0 && s.saturation == 0.0 && s.luminance == 0.0;
    if shifts.iter().all(is_zero) {
        return img.clone();
    }
    let zero: [bool; 8] = std::array::from_fn(|i| is_zero(&shifts[i]));

    let mut out = img.clone();
    for px in out.pixels_mut() {
        let Some((h, s, l)) = rgb_to_hsl(px.0[0], px.0[1], px.0[2]) else {
            // 無彩色画素は色相を持たないので触れない。
            continue;
        };
        let ((i, wi), (j, wj)) = band_weights(h);
        if zero[i] && zero[j] {
            // 寄与する 2 帯がともにゼロシフト → 変換往復を通さずそのまま残す。
            continue;
        }

        let dh = (wi * shifts[i].hue + wj * shifts[j].hue) * HUE_DEGREES_PER_UNIT;
        let f_sat = ((wi * shifts[i].saturation + wj * shifts[j].saturation) / 100.0)
            .clamp(FACTOR_MIN, FACTOR_MAX);
        let f_lum = ((wi * shifts[i].luminance + wj * shifts[j].luminance) / 100.0
            * LUMINANCE_SCALE)
            .clamp(FACTOR_MIN, FACTOR_MAX);

        let nh = h + dh;
        let ns = (s * (1.0 + f_sat)).clamp(0.0, 1.0);
        let nl = (l * (1.0 + f_lum)).clamp(0.0, 1.0);

        let rgb = hsl_to_rgb(nh, ns, nl);
        px.0[0] = rgb[0];
        px.0[1] = rgb[1];
        px.0[2] = rgb[2];
        // px.0[3](アルファ)は意図的に触れない。
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_weights_sum_to_one_and_peak_at_centers() {
        for (i, c) in BAND_CENTERS.iter().enumerate() {
            let ((a, wa), (_, wb)) = band_weights(*c);
            assert_eq!(a, i);
            assert_eq!(wa, 1.0, "center {c} must have weight 1");
            assert_eq!(wb, 0.0);
        }
        let mut h = 0.0;
        while h < 360.0 {
            let ((_, wa), (_, wb)) = band_weights(h);
            assert!(
                (wa + wb - 1.0).abs() < 1e-12,
                "weights at {h} must sum to 1"
            );
            assert!(wa >= 0.0 && wb >= 0.0);
            h += 0.25;
        }
    }

    #[test]
    fn primaries_map_to_expected_hues() {
        assert_eq!(rgb_to_hsl(255, 0, 0).unwrap().0, 0.0);
        assert_eq!(rgb_to_hsl(0, 255, 0).unwrap().0, 120.0);
        assert_eq!(rgb_to_hsl(0, 0, 255).unwrap().0, 240.0);
        assert_eq!(rgb_to_hsl(255, 255, 0).unwrap().0, 60.0);
        assert!(rgb_to_hsl(77, 77, 77).is_none(), "grey has no hue");
    }

    /// 網羅の縮小版(step 17)。フル網羅は tests/wb_hsl_ops.rs 側の品質ゲート。
    #[test]
    fn roundtrip_is_exact_on_coarse_grid() {
        for r in (0..=255u32).step_by(17) {
            for g in (0..=255u32).step_by(17) {
                for b in (0..=255u32).step_by(17) {
                    let (r, g, b) = (r as u8, g as u8, b as u8);
                    let Some((h, s, l)) = rgb_to_hsl(r, g, b) else {
                        continue;
                    };
                    assert_eq!(hsl_to_rgb(h, s, l), [r, g, b], "roundtrip {r},{g},{b}");
                }
            }
        }
    }
}
