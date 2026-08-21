//! color_matrix / curves / levels(トーン・カラー系メタ op)。
//!
//! 実装規約:
//! - `color_matrix`: 0..1 正規化 f64 で行列適用 → クランプ → u8 へ half-away-from-zero 丸め。
//!   行列値はレシピ由来(canonical 量子化済み)なので追加量子化は不要
//! - `curves`: 制御点を Fritsch–Carlson 単調3次補間で 256 LUT 化(f64 計算 → 丸め)。
//!   x 重複はエラー。点が1個なら定数、0個(None)は恒等
//! - `levels`: in/out レンジ + gamma を 256 LUT に落とし curves と同じ適用経路を通す
//!
//! # アルファの扱い(op ごとに異なるので注意)
//! - `curves` / `levels`: **アルファには一切触れない**(トーン補正は色にのみ作用する。
//!   不透明度をカーブで動かしたい要求は将来 mask/blend の語彙で扱う。Phase C/D)
//! - `color_matrix`: 4×5 行列は 4 行目がアルファ行なので **アルファも変換対象**。
//!   恒等行列(4行目 = [0,0,0,1,0])ならアルファは保存される
//!
//! # 決定論
//! ops/mod.rs の規約どおり、libm 由来の関数(`powf` 等)の結果を画素へ直接持ち込まない。
//! トーン系は必ず 256 エントリ LUT を経由し、画素ループは配列引きに閉じる。
//! LUT 生成時の f64 値は最終丸めの直前に 1e-9 グリッドへ量子化する(理由は
//! `quantize_round_u8` のドキュメント参照)。

use image::RgbaImage;

use crate::{AtxError::InvalidRecipe, Result};

/// 行列要素の絶対値上限。これを超える係数は「桁を間違えたレシピ」とみなす
/// (通常の色行列は -2..2 程度に収まる。8.0 は強めのチャンネルミキサーでも足りる余裕)。
const MATRIX_ABS_MAX: f64 = 8.0;

/// 1チャンネルあたりの制御点数の上限。
const MAX_CURVE_POINTS: usize = 32;

/// gamma の許容範囲。
const GAMMA_MIN: f64 = 0.1;
const GAMMA_MAX: f64 = 10.0;

// ----------------------------------------------------------------- validate

/// 4×5 行列の静的検証: 長さ 20 / 全要素が有限 / |v| <= 8.0。
pub fn validate_matrix(index: usize, matrix: &[f64]) -> Result<()> {
    if matrix.len() != 20 {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (color_matrix): matrix must have exactly 20 elements \
             (4 rows x 5 columns, row-major), got {}",
            matrix.len()
        )));
    }
    for (i, v) in matrix.iter().enumerate() {
        if !v.is_finite() {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (color_matrix): matrix[{i}] must be finite, got {v}"
            )));
        }
        if v.abs() > MATRIX_ABS_MAX {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (color_matrix): matrix[{i}] must be within \
                 -{MATRIX_ABS_MAX}..={MATRIX_ABS_MAX}, got {v}"
            )));
        }
    }
    Ok(())
}

/// curves の静的検証。
///
/// - 4 チャンネル(master/red/green/blue)すべて未指定は無意味なのでエラー
/// - 各チャンネルの制御点は 1..=32 個。**1 個は定数 LUT**(その y で塗り潰す)として許可する
/// - x は狭義単調増加。重複 x はエラー(どちらの y を採るかが決まらないため)
/// - y は 0-255 の任意値(反転カーブ・非単調カーブも許可する)
#[allow(clippy::type_complexity)]
pub fn validate_curves(
    index: usize,
    master: &Option<Vec<[u8; 2]>>,
    red: &Option<Vec<[u8; 2]>>,
    green: &Option<Vec<[u8; 2]>>,
    blue: &Option<Vec<[u8; 2]>>,
) -> Result<()> {
    let channels: [(&str, &Option<Vec<[u8; 2]>>); 4] = [
        ("master", master),
        ("red", red),
        ("green", green),
        ("blue", blue),
    ];
    if channels.iter().all(|(_, c)| c.is_none()) {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (curves): at least one of master, red, green, blue is required"
        )));
    }
    for (name, channel) in channels {
        let Some(points) = channel else { continue };
        if points.is_empty() || points.len() > MAX_CURVE_POINTS {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (curves): {name} must have 1..={MAX_CURVE_POINTS} control \
                 points, got {}",
                points.len()
            )));
        }
        for pair in points.windows(2) {
            if pair[0][0] >= pair[1][0] {
                return Err(InvalidRecipe(format!(
                    "operations[{index}] (curves): {name} control point x values must be strictly \
                     increasing, got {} after {}",
                    pair[1][0], pair[0][0]
                )));
            }
        }
    }
    Ok(())
}

/// levels の静的検証: in_black < in_white / out_black <= out_white / gamma は有限かつ 0.1..=10.0。
///
/// out レンジは反転を許さない代わりに潰し(out_black == out_white)は許可する
/// (単色塗り潰しは合法な指定)。in レンジは 0 除算になるため潰しを許さない。
pub fn validate_levels(
    index: usize,
    in_black: u8,
    in_white: u8,
    gamma: f64,
    out_black: u8,
    out_white: u8,
) -> Result<()> {
    if in_black >= in_white {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (levels): in_black must be less than in_white, \
             got in_black={in_black}, in_white={in_white}"
        )));
    }
    if out_black > out_white {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (levels): out_black must be less than or equal to out_white, \
             got out_black={out_black}, out_white={out_white}"
        )));
    }
    if !gamma.is_finite() || !(GAMMA_MIN..=GAMMA_MAX).contains(&gamma) {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (levels): gamma must be a finite value within \
             {GAMMA_MIN}..={GAMMA_MAX}, got {gamma}"
        )));
    }
    Ok(())
}

// ------------------------------------------------------------------- 共通部品

/// f64 の計算結果を u8 へ落とす共通経路。
///
/// 1. 1e-9 グリッドへ量子化する
/// 2. 0..255 へクランプ
/// 3. half-away-from-zero 丸め(`f64::round` の規則)
///
/// **1e-9 量子化の理由**: `powf` などの libm 実装はプラットフォーム間で 1 ULP 差を持ちうる。
/// 最終結果は 256 段の u8 なので通常この差は吸収されるが、真値がちょうど `.5` 境界に乗る
/// ケースでは 1 ULP の差が丸め方向を反転させ、出力バイトが変わってしまう。
/// 丸め直前に 1e-9 グリッドへ寄せておけば 1 ULP 程度のずれは同じ格子点に落ち、
/// 境界の反転が起きない(recipe.rs の canonical 量子化と同じ思想。ops/mod.rs 規約)。
fn quantize_round_u8(v: f64) -> u8 {
    let quantized = (v * 1e9).round() / 1e9;
    quantized.clamp(0.0, 255.0).round() as u8
}

/// 恒等 LUT(`lut[i] == i`)。
fn identity_lut() -> [u8; 256] {
    let mut lut = [0u8; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        *slot = i as u8;
    }
    lut
}

/// 制御点列から 256 エントリ LUT を作る(Fritsch–Carlson 単調3次補間)。
///
/// # 補間
/// 各区間の傾き `d[k] = (y[k+1]-y[k]) / (x[k+1]-x[k])` を求め、接線 `m` を
/// 端点は片側差分、内点は隣接区間傾きの平均で初期化する。その後 Fritsch–Carlson の
/// フィルタを掛けて単調性を保証する:
/// - `d[k] == 0` の区間は両端の接線を 0 にする(平坦区間の行き過ぎ防止)
/// - `alpha = m[k]/d[k]`, `beta = m[k+1]/d[k]` が負なら 0 に落とす(符号反転の抑制)
/// - `alpha^2 + beta^2 > 9` なら `tau = 3/sqrt(alpha^2+beta^2)` で両接線を縮める
///
/// これにより **制御点が単調非減少なら LUT も単調非減少** になる(overshoot なし)。
/// 区間内は3次 Hermite 基底で評価する。
///
/// # 端の外挿
/// 最初の点の x が 0 より大きい場合、`x < x[0]` の入力はすべて **最初の点の y** を返す
/// (外挿ではなく定数クランプ)。同様に最後の点の x が 255 未満なら、`x > x[last]` は
/// **最後の点の y** で一定。カーブパネルの端点固定と同じ挙動。
///
/// # 特殊ケース
/// - 点が 0 個: 恒等 LUT(validate では到達しないが安全側の既定)
/// - 点が 1 個: その y で埋めた定数 LUT
fn lut_from_points(points: &[[u8; 2]]) -> [u8; 256] {
    let n = points.len();
    if n == 0 {
        return identity_lut();
    }
    if n == 1 {
        return [points[0][1]; 256];
    }

    let xs: Vec<f64> = points.iter().map(|p| p[0] as f64).collect();
    let ys: Vec<f64> = points.iter().map(|p| p[1] as f64).collect();

    // 区間傾き(validate により x は狭義単調増加なので分母は 0 にならない)。
    let mut d = vec![0.0f64; n - 1];
    for k in 0..n - 1 {
        d[k] = (ys[k + 1] - ys[k]) / (xs[k + 1] - xs[k]);
    }

    // 接線の初期化: 端点は片側差分、内点は隣接区間の平均。
    let mut m = vec![0.0f64; n];
    m[0] = d[0];
    m[n - 1] = d[n - 2];
    for k in 1..n - 1 {
        m[k] = (d[k - 1] + d[k]) / 2.0;
    }

    // Fritsch–Carlson フィルタ。
    for k in 0..n - 1 {
        if d[k] == 0.0 {
            m[k] = 0.0;
            m[k + 1] = 0.0;
            continue;
        }
        if m[k] / d[k] < 0.0 {
            m[k] = 0.0;
        }
        if m[k + 1] / d[k] < 0.0 {
            m[k + 1] = 0.0;
        }
        let alpha = m[k] / d[k];
        let beta = m[k + 1] / d[k];
        let s = alpha * alpha + beta * beta;
        if s > 9.0 {
            let tau = 3.0 / s.sqrt();
            m[k] = tau * alpha * d[k];
            m[k + 1] = tau * beta * d[k];
        }
    }

    let mut lut = [0u8; 256];
    // 入力 x は 0..256 の昇順なので、区間インデックスは前進のみで足りる。
    let mut seg = 0usize;
    for (i, slot) in lut.iter_mut().enumerate() {
        let x = i as f64;
        let y = if x <= xs[0] {
            ys[0]
        } else if x >= xs[n - 1] {
            ys[n - 1]
        } else {
            while seg + 2 < n && xs[seg + 1] <= x {
                seg += 1;
            }
            let h = xs[seg + 1] - xs[seg];
            let t = (x - xs[seg]) / h;
            let t2 = t * t;
            let t3 = t2 * t;
            // 3次 Hermite 基底(演算順は決定論のため固定。再結合しない)。
            let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
            let h10 = t3 - 2.0 * t2 + t;
            let h01 = -2.0 * t3 + 3.0 * t2;
            let h11 = t3 - t2;
            h00 * ys[seg] + h10 * h * m[seg] + h01 * ys[seg + 1] + h11 * h * m[seg + 1]
        };
        *slot = quantize_round_u8(y);
    }
    lut
}

/// `first` を適用してから `second` を適用する合成 LUT。
fn compose(first: &[u8; 256], second: &[u8; 256]) -> [u8; 256] {
    let mut out = [0u8; 256];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = second[first[i] as usize];
    }
    out
}

/// R/G/B に個別 LUT、A は素通しで適用する。
fn apply_rgb_luts(img: &RgbaImage, luts: &[[u8; 256]; 3]) -> RgbaImage {
    let mut out = img.clone();
    for px in out.pixels_mut() {
        for (i, lut) in luts.iter().enumerate() {
            px.0[i] = lut[px.0[i] as usize];
        }
        // px.0[3](アルファ)は意図的に触れない。
    }
    out
}

// ----------------------------------------------------------------------- op

/// 4×5 行列(長さ20、行優先)を適用する。alpha も行列の対象。
///
/// 各画素の RGBA を 0..1 の f64 に正規化し
/// `R' = m[0]R + m[1]G + m[2]B + m[3]A + m[4]`(以下 G'/B'/A' も同様に行ごと)
/// を計算、0..1 にクランプして u8 へ half-away-from-zero 丸めする。
///
/// 行列値は canonical 量子化済みのレシピ由来なので追加量子化は行わない。
/// 演算は libm を含まない加減乗のみで、Rust は浮動小数の再結合を行わないため
/// 記述順がそのまま評価順になり結果は決定論的。
///
/// 長さが 20 でない場合(validate を通っていれば起こらない)は入力をそのまま返す。
pub fn color_matrix(img: &RgbaImage, matrix: &[f64]) -> RgbaImage {
    if matrix.len() != 20 {
        return img.clone();
    }
    let m: &[f64; 20] = matrix.try_into().expect("length checked above");

    let mut out = img.clone();
    for px in out.pixels_mut() {
        let r = px.0[0] as f64 / 255.0;
        let g = px.0[1] as f64 / 255.0;
        let b = px.0[2] as f64 / 255.0;
        let a = px.0[3] as f64 / 255.0;

        // 行ごとに同じ順序で積和する(再結合禁止)。
        let nr = m[0] * r + m[1] * g + m[2] * b + m[3] * a + m[4];
        let ng = m[5] * r + m[6] * g + m[7] * b + m[8] * a + m[9];
        let nb = m[10] * r + m[11] * g + m[12] * b + m[13] * a + m[14];
        let na = m[15] * r + m[16] * g + m[17] * b + m[18] * a + m[19];

        for (i, v) in [nr, ng, nb, na].into_iter().enumerate() {
            px.0[i] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
    out
}

/// チャンネル別トーンカーブ。
///
/// 適用順は **master → 各チャンネル**。master は R/G/B に等しく掛かり、
/// アルファには掛からない。画素ループに入る前に master と各チャンネルの LUT を
/// 1 本ずつに合成する(`composed[i] = channel[master[i]]`)ので、
/// 画素あたりのコストは LUT 引き 3 回、中間丸めも 1 回だけになる。
///
/// 未指定(None)のチャンネルは恒等 LUT として扱う。
#[allow(clippy::type_complexity)]
pub fn curves(
    img: &RgbaImage,
    master: &Option<Vec<[u8; 2]>>,
    red: &Option<Vec<[u8; 2]>>,
    green: &Option<Vec<[u8; 2]>>,
    blue: &Option<Vec<[u8; 2]>>,
) -> RgbaImage {
    let lut_of = |c: &Option<Vec<[u8; 2]>>| match c {
        Some(points) => lut_from_points(points),
        None => identity_lut(),
    };
    let master_lut = lut_of(master);
    let luts = [
        compose(&master_lut, &lut_of(red)),
        compose(&master_lut, &lut_of(green)),
        compose(&master_lut, &lut_of(blue)),
    ];
    apply_rgb_luts(img, &luts)
}

/// レベル補正。R/G/B に同一 LUT を適用し、アルファには触れない。
///
/// LUT の定義(入力 v は 0..255 の整数):
/// 1. `v01 = clamp((v - in_black) / (in_white - in_black), 0, 1)`
/// 2. `v01 = v01 ^ (1 / gamma)`(gamma > 1 で明るく、< 1 で暗く)
/// 3. `out = out_black + v01 * (out_white - out_black)`
/// 4. 1e-9 グリッドへ量子化してから half-away-from-zero 丸め
///
/// `powf` は libm なのでプラットフォーム差を持ちうるが、結果は 256 エントリの
/// LUT に落ちた時点で量子化される。境界反転だけが残るリスクなので
/// `quantize_round_u8` で潰す(同関数のドキュメント参照)。
/// 画素ループは純粋な LUT 引き。
pub fn levels(
    img: &RgbaImage,
    in_black: u8,
    in_white: u8,
    gamma: f64,
    out_black: u8,
    out_white: u8,
) -> RgbaImage {
    let lut = levels_lut(in_black, in_white, gamma, out_black, out_white);
    let luts = [lut, lut, lut];
    apply_rgb_luts(img, &luts)
}

/// levels の 256 エントリ LUT を組む。
fn levels_lut(in_black: u8, in_white: u8, gamma: f64, out_black: u8, out_white: u8) -> [u8; 256] {
    let lo = in_black as f64;
    // validate で in_black < in_white を保証しているので分母は正。
    // 万一 0 以下でも 1.0 に退避して 0 除算・NaN を避ける。
    let span = {
        let s = in_white as f64 - lo;
        if s > 0.0 {
            s
        } else {
            1.0
        }
    };
    let out_lo = out_black as f64;
    let out_span = out_white as f64 - out_lo;
    let exponent = 1.0 / gamma;

    let mut lut = [0u8; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        let v01 = ((i as f64 - lo) / span).clamp(0.0, 1.0);
        // gamma == 1.0 は powf を通さない(libm を無駄に踏まない + 恒等を厳密に保つ)。
        let shaped = if gamma == 1.0 {
            v01
        } else {
            v01.powf(exponent)
        };
        *slot = quantize_round_u8(out_lo + shaped * out_span);
    }
    lut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_curve_points_give_identity_lut() {
        let lut = lut_from_points(&[[0, 0], [255, 255]]);
        assert_eq!(lut, identity_lut());
    }

    #[test]
    fn single_point_gives_constant_lut() {
        let lut = lut_from_points(&[[128, 77]]);
        assert!(lut.iter().all(|&v| v == 77));
    }

    #[test]
    fn endpoints_clamp_outside_control_range() {
        let lut = lut_from_points(&[[10, 20], [200, 240]]);
        assert_eq!(lut[0], 20);
        assert_eq!(lut[9], 20);
        assert_eq!(lut[200], 240);
        assert_eq!(lut[255], 240);
    }

    #[test]
    fn levels_default_lut_is_identity() {
        let lut = levels_lut(0, 255, 1.0, 0, 255);
        assert_eq!(lut, identity_lut());
    }
}
