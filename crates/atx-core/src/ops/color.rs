//! color_matrix / curves / levels(トーン・カラー系メタ op)。
//!
//! **作業空間: sRGB 符号値**(`ops/mod.rs` の表を参照)。制御点座標 0-255 や
//! 行列係数の慣習はいずれも符号値上で定義されているため、線形光では適用しない。
//!
//! 実装規約:
//! - `color_matrix`: 0..1 の符号値 f64 で行列適用 → 0..1 へクランプ。
//!   行列値はレシピ由来(canonical 量子化済み)なので追加量子化は不要
//! - `curves`: 制御点を Fritsch–Carlson 単調3次補間で **256 ノードの区分線形関数**へ。
//!   x 重複はエラー。点が1個なら定数、0個(None)は恒等
//! - `levels`: in/out レンジ + gamma を同じ 256 ノード表に落とし curves と同じ経路を通す
//!
//! # v2 の変更点: LUT の値は u8 に丸めない
//!
//! v1 の LUT は `[u8; 256]` で、画素も u8 だったため、トーン系 op を重ねるたびに
//! 256 段へ再量子化されポスタリゼーションが蓄積した(「持ち上げて戻す」を繰り返すと
//! 階調が櫛の歯状に欠ける)。v2 では
//!
//! - **ノード位置** は従来どおり u8 格子(`x = i / 255`, `i = 0..=255`)。制御点の
//!   意味論・`recipe_hash` は 1 ビットも変わらない
//! - **ノード値** は f32(0..1)。u8 丸めを一切しない
//! - ノード間は **f32 の線形補間**(演算順序固定・FMA 不使用)
//!
//! とすることで、f32 の精度がトーンスタック全体を通して保たれる
//! (`tests/engine_v2_quality.rs` の「交互に 8 回かけて元に戻る」テストが硬いゲート)。
//!
//! # アルファの扱い(op ごとに異なるので注意)
//! - `curves` / `levels`: **アルファには一切触れない**(トーン補正は色にのみ作用する)
//! - `color_matrix`: 4×5 行列は 4 行目がアルファ行なので **アルファも変換対象**。
//!   恒等行列(4行目 = [0,0,0,1,0])ならアルファは保存される
//!
//! # 決定論
//! ops/mod.rs の規約どおり、libm 由来の関数(`powf` 等)の結果を画素へ直接持ち込まない。
//! トーン系は必ず 256 ノード表を経由し、画素ループは配列引きと線形補間に閉じる。
//! 表の生成時の f64 値は 1e-9 グリッドへ量子化してから f32 化する。

use crate::linear::{quantize_1e9, LinearImage};
use crate::parallel;
use crate::{AtxError::InvalidRecipe, Result};

/// トーン表のノード数(u8 格子)。
const TONE_NODES: usize = 256;
/// 補間で使う最大ノード添字(f32 定数)。
const TONE_LAST: f32 = (TONE_NODES - 1) as f32;

/// 256 ノードの区分線形トーン関数(定義域・値域とも 0..1)。
type ToneTable = [f32; TONE_NODES];

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

/// 0-255 スケールの f64 をトーン表のノード値(0..1 の f32)へ落とす共通経路。
///
/// 1. 1e-9 グリッドへ量子化する(`powf` 等の libm 差の遮断。ops/mod.rs 規約)
/// 2. 0..255 へクランプ
/// 3. 255 で割って 0..1 へ
///
/// v1 と違い **u8 への丸めは行わない**(モジュールドキュメント参照)。
fn node_value(v: f64) -> f32 {
    let quantized = quantize_1e9(v);
    (quantized.clamp(0.0, 255.0) / 255.0) as f32
}

/// 恒等トーン表(`table[i] == i / 255`)。
fn identity_table() -> ToneTable {
    let mut table = [0f32; TONE_NODES];
    for (i, slot) in table.iter_mut().enumerate() {
        *slot = i as f32 / TONE_LAST;
    }
    table
}

/// トーン表を線形補間で引く(演算順序固定・FMA 不使用)。
#[inline]
fn eval(table: &ToneTable, v: f32) -> f32 {
    let pos = v.clamp(0.0, 1.0) * TONE_LAST;
    let floor = pos.floor();
    let i0 = floor as usize;
    if i0 >= TONE_NODES - 1 {
        return table[TONE_NODES - 1];
    }
    let frac = pos - floor;
    let a = table[i0];
    let b = table[i0 + 1];
    let delta = b - a;
    let step = delta * frac;
    a + step
}

/// 制御点列から 256 ノードのトーン表を作る(Fritsch–Carlson 単調3次補間)。
///
/// # 補間
/// 各区間の傾き `d[k] = (y[k+1]-y[k]) / (x[k+1]-x[k])` を求め、接線 `m` を
/// 端点は片側差分、内点は隣接区間傾きの平均で初期化する。その後 Fritsch–Carlson の
/// フィルタを掛けて単調性を保証する:
/// - `d[k] == 0` の区間は両端の接線を 0 にする(平坦区間の行き過ぎ防止)
/// - `alpha = m[k]/d[k]`, `beta = m[k+1]/d[k]` が負なら 0 に落とす(符号反転の抑制)
/// - `alpha^2 + beta^2 > 9` なら `tau = 3/sqrt(alpha^2+beta^2)` で両接線を縮める
///
/// これにより **制御点が単調非減少なら表も単調非減少** になる(overshoot なし)。
/// 区間内は3次 Hermite 基底で評価する。
///
/// # 端の外挿
/// 最初の点の x が 0 より大きい場合、`x < x[0]` の入力はすべて **最初の点の y** を返す
/// (外挿ではなく定数クランプ)。同様に最後の点の x が 255 未満なら、`x > x[last]` は
/// **最後の点の y** で一定。カーブパネルの端点固定と同じ挙動。
///
/// # 特殊ケース
/// - 点が 0 個: 恒等表(validate では到達しないが安全側の既定)
/// - 点が 1 個: その y で埋めた定数表
fn table_from_points(points: &[[u8; 2]]) -> ToneTable {
    let n = points.len();
    if n == 0 {
        return identity_table();
    }
    if n == 1 {
        return [node_value(points[0][1] as f64); TONE_NODES];
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

    let mut table = [0f32; TONE_NODES];
    // 入力 x は 0..256 の昇順なので、区間インデックスは前進のみで足りる。
    let mut seg = 0usize;
    for (i, slot) in table.iter_mut().enumerate() {
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
        *slot = node_value(y);
    }
    table
}

/// `first` を適用してから `second` を適用する合成トーン表。
///
/// 合成後もノード位置は u8 格子のままなので、区間内では
/// 「first が線形 → second が線形」の合成を 1 本の線形で近似することになる。
/// 制御点の意味論(ノード上での値)は v1 と同一で、画素あたりの補間も 1 回で済む。
fn compose(first: &ToneTable, second: &ToneTable) -> ToneTable {
    let mut out = [0f32; TONE_NODES];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = eval(second, first[i]);
    }
    out
}

/// R/G/B に個別のトーン表、A は素通しで適用する(**sRGB 符号値空間**)。
fn apply_rgb_tables(img: &LinearImage, tables: &[ToneTable; 3]) -> LinearImage {
    let mut out = img.clone();
    parallel::for_each_chunk(&mut out.data, |chunk| {
        for px in chunk.iter_mut() {
            for (i, table) in tables.iter().enumerate() {
                px[i] = eval(table, px[i]);
            }
            // px[3](アルファ)は意図的に触れない。
        }
    });
    out
}

// ----------------------------------------------------------------------- op

/// 4×5 行列(長さ20、行優先)を適用する。alpha も行列の対象。
///
/// 各画素の RGBA(**sRGB 符号値 0..1**)を f64 に上げて
/// `R' = m[0]R + m[1]G + m[2]B + m[3]A + m[4]`(以下 G'/B'/A' も同様に行ごと)
/// を計算、0..1 にクランプして f32 へ戻す。
///
/// 行列値は canonical 量子化済みのレシピ由来なので追加量子化は行わない。
/// 演算は libm を含まない加減乗のみで、Rust は浮動小数の再結合を行わないため
/// 記述順がそのまま評価順になり結果は決定論的。
///
/// 長さが 20 でない場合(validate を通っていれば起こらない)は入力をそのまま返す。
pub fn color_matrix(img: &LinearImage, matrix: &[f64]) -> LinearImage {
    if matrix.len() != 20 {
        return img.clone();
    }
    let m: &[f64; 20] = matrix.try_into().expect("length checked above");

    let mut out = img.clone();
    parallel::for_each_chunk(&mut out.data, |chunk| {
        for px in chunk.iter_mut() {
            let r = px[0] as f64;
            let g = px[1] as f64;
            let b = px[2] as f64;
            let a = px[3] as f64;

            // 行ごとに同じ順序で積和する(再結合禁止)。
            let nr = m[0] * r + m[1] * g + m[2] * b + m[3] * a + m[4];
            let ng = m[5] * r + m[6] * g + m[7] * b + m[8] * a + m[9];
            let nb = m[10] * r + m[11] * g + m[12] * b + m[13] * a + m[14];
            let na = m[15] * r + m[16] * g + m[17] * b + m[18] * a + m[19];

            for (i, v) in [nr, ng, nb, na].into_iter().enumerate() {
                px[i] = v.clamp(0.0, 1.0) as f32;
            }
        }
    });
    out
}

/// チャンネル別トーンカーブ(**sRGB 符号値空間**)。
///
/// 適用順は **master → 各チャンネル**。master は R/G/B に等しく掛かり、
/// アルファには掛からない。画素ループに入る前に master と各チャンネルの表を
/// 1 本ずつに合成する(`composed[i] = channel(master[i])`)ので、
/// 画素あたりのコストは補間付き表引き 3 回になる。
///
/// 未指定(None)のチャンネルは恒等表として扱う。
#[allow(clippy::type_complexity)]
pub fn curves(
    img: &LinearImage,
    master: &Option<Vec<[u8; 2]>>,
    red: &Option<Vec<[u8; 2]>>,
    green: &Option<Vec<[u8; 2]>>,
    blue: &Option<Vec<[u8; 2]>>,
) -> LinearImage {
    let table_of = |c: &Option<Vec<[u8; 2]>>| match c {
        Some(points) => table_from_points(points),
        None => identity_table(),
    };
    let master_table = table_of(master);
    let tables = [
        compose(&master_table, &table_of(red)),
        compose(&master_table, &table_of(green)),
        compose(&master_table, &table_of(blue)),
    ];
    apply_rgb_tables(img, &tables)
}

/// レベル補正(**sRGB 符号値空間**)。R/G/B に同一表を適用し、アルファには触れない。
///
/// 表の定義(ノード添字 i = 0..=255 が入力 0..1 の u8 格子に対応):
/// 1. `v01 = clamp((i - in_black) / (in_white - in_black), 0, 1)`
/// 2. `v01 = v01 ^ (1 / gamma)`(gamma > 1 で明るく、< 1 で暗く)
/// 3. `out = out_black + v01 * (out_white - out_black)`
/// 4. 1e-9 グリッドへ量子化してから 0..1 へ正規化(u8 丸めはしない)
///
/// `powf` は libm なのでプラットフォーム差を持ちうるが、結果は表の生成時に
/// 1e-9 グリッドへ量子化される。画素ループは表引き + 線形補間のみ。
pub fn levels(
    img: &LinearImage,
    in_black: u8,
    in_white: u8,
    gamma: f64,
    out_black: u8,
    out_white: u8,
) -> LinearImage {
    let table = levels_table(in_black, in_white, gamma, out_black, out_white);
    let tables = [table, table, table];
    apply_rgb_tables(img, &tables)
}

/// levels の 256 ノード表を組む。
fn levels_table(in_black: u8, in_white: u8, gamma: f64, out_black: u8, out_white: u8) -> ToneTable {
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

    let mut table = [0f32; TONE_NODES];
    for (i, slot) in table.iter_mut().enumerate() {
        let v01 = ((i as f64 - lo) / span).clamp(0.0, 1.0);
        // gamma == 1.0 は powf を通さない(libm を無駄に踏まない + 恒等を厳密に保つ)。
        let shaped = if gamma == 1.0 {
            v01
        } else {
            v01.powf(exponent)
        };
        *slot = node_value(out_lo + shaped * out_span);
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_curve_points_give_identity_table() {
        let table = table_from_points(&[[0, 0], [255, 255]]);
        assert_eq!(table, identity_table());
    }

    #[test]
    fn single_point_gives_constant_table() {
        let table = table_from_points(&[[128, 77]]);
        let expected = 77.0f32 / 255.0;
        assert!(table.iter().all(|&v| v == expected));
    }

    #[test]
    fn endpoints_clamp_outside_control_range() {
        let table = table_from_points(&[[10, 20], [200, 240]]);
        assert_eq!(table[0], 20.0 / 255.0);
        assert_eq!(table[9], 20.0 / 255.0);
        assert_eq!(table[200], 240.0 / 255.0);
        assert_eq!(table[255], 240.0 / 255.0);
    }

    #[test]
    fn levels_default_table_is_identity() {
        let table = levels_table(0, 255, 1.0, 0, 255);
        assert_eq!(table, identity_table());
    }

    /// 直線カーブ(制御点 2 個)は区分線形表として厳密に再現され、
    /// その逆関数と合成すると恒等に戻る(f32 の精度内)。
    /// v1 の u8 表では 0.5 段の量子化誤差が残っていた性質。
    #[test]
    fn straight_curve_and_its_inverse_compose_to_identity() {
        let up = table_from_points(&[[0, 32], [255, 255]]);
        let down = table_from_points(&[[32, 0], [255, 255]]);
        for i in 0..=255u32 {
            let x = i as f32 / 255.0;
            let y = eval(&down, eval(&up, x));
            assert!((y - x).abs() < 1e-5, "roundtrip at {i}: {x} -> {y}");
        }
    }
}
