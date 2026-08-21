//! perspective(台形/キーストーン補正)。
//!
//! 形式は排他: `quad`(入力画像内の四角形 → 出力長方形)または
//! `vertical_degrees`/`horizontal_degrees`(キーストーン角、併用可)。
//!
//! transform.rs との連動: 射影変換はアフィンで表現できないため、
//! この op の実装は transform.rs の変換追跡を 3x3 射影行列へ拡張し、
//! `coordinate_space: "source"` の crop が perspective 越しでも正しく写像されるようにする
//! (アフィンは射影の部分集合なので既存 op の追跡はそのまま埋め込める)。
//!
//! # 座標系
//!
//! quad の座標も、返す変換も **連続座標**(画素 index `i` は区間 `[i, i+1)`、
//! 画素中心は `i + 0.5`)で扱う。`recipe::Rect` と同じ約束であり、
//! `transform` モジュールの解説を参照。
//!
//! # 決定論(`ops/mod.rs` の規約)
//!
//! libm 由来の値(`tan` / `sqrt`)は求めた直後に 1e-6 グリッドへ量子化し、
//! そこから先の行列組み立ては四則演算のみ(IEEE で厳密に定義され、
//! プラットフォーム差が出ない)で行う。さらに行列そのものも
//! **画像を単位長に正規化した座標系**(1 単位 = `max(W, H)` 画素)で組んでから
//! 全 9 要素を 1e-6 グリッドへ量子化する。この座標系では全係数が O(1) なので、
//! 量子化の誤差は相対 1e-6 = 4000px 画像でも 0.01 画素未満に収まる
//! (画素座標のまま量子化すると、最下行の係数が 1e-4 程度で相対誤差が
//! 1e-2 まで悪化し、端で 1 画素近くずれてしまう)。
//! 正規化座標 → 画素座標の戻しは対角スケール行列との合成、つまり四則演算のみ。

use image::Rgba;
use imageproc::geometric_transformations::{warp_into, Border, Interpolation, Projection};

use crate::linear::{pad_to_linear, LinearImage};
use crate::pixel_ops::{from_f32_image, to_f32_image, F32Image};
use crate::recipe::parse_hex_color;
use crate::transform::{quantize_1e6, Transform};
use crate::{AtxError, Result};

/// 既定のパディング色(白・不透明)。engine の `DEFAULT_PAD` と同じ。
const DEFAULT_PAD: [u8; 4] = [255, 255, 255, 255];

/// キーストーン角の上限(度)。これを超えると補正の引き伸ばしが実用に耐えない上、
/// 透視除算の分母が 0 に近づいて写像が退化する。
const MAX_KEYSTONE_DEGREES: f64 = 45.0;

#[allow(clippy::type_complexity)]
pub fn validate(
    index: usize,
    quad: &Option<[[f64; 2]; 4]>,
    vertical_degrees: &Option<f64>,
    horizontal_degrees: &Option<f64>,
    pad_color: &Option<String>,
) -> Result<()> {
    use crate::AtxError::InvalidRecipe;

    let has_keystone = vertical_degrees.is_some() || horizontal_degrees.is_some();
    match (quad.is_some(), has_keystone) {
        (true, true) => {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (perspective): specify exactly one form — either quad \
                 or vertical_degrees/horizontal_degrees, not both"
            )));
        }
        (false, false) => {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (perspective): one form is required — quad \
                 (4 points, tl/tr/br/bl) or vertical_degrees/horizontal_degrees"
            )));
        }
        _ => {}
    }

    if let Some(q) = quad {
        for (i, p) in q.iter().enumerate() {
            if !p[0].is_finite() || !p[1].is_finite() {
                return Err(InvalidRecipe(format!(
                    "operations[{index}] (perspective): quad[{i}] has a non-finite coordinate \
                     ({}, {})",
                    p[0], p[1]
                )));
            }
        }
        if !is_strictly_convex_in_order(q) {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (perspective): quad must be a strictly convex quadrilateral \
                 listed as tl, tr, br, bl (clockwise on screen, y pointing down); \
                 got {q:?} which is concave, self-intersecting, degenerate, or in the wrong order"
            )));
        }
    }

    for (name, deg) in [
        ("vertical_degrees", vertical_degrees),
        ("horizontal_degrees", horizontal_degrees),
    ] {
        if let Some(d) = deg {
            if !d.is_finite() || d.abs() > MAX_KEYSTONE_DEGREES {
                return Err(InvalidRecipe(format!(
                    "operations[{index}] (perspective): {name} must be within \
                     -{MAX_KEYSTONE_DEGREES}..={MAX_KEYSTONE_DEGREES}, got {d}"
                )));
            }
        }
    }

    if let Some(c) = pad_color {
        if parse_hex_color(c).is_none() {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (perspective): pad_color must be a CSS hex color \
                 (\"#rgb\" / \"#rrggbb\" / \"#rrggbbaa\"), got {c:?}"
            )));
        }
    }

    Ok(())
}

/// tl, tr, br, bl の順に並んだ**厳密に凸**な四角形か。
///
/// 連続する 3 点の外積 z 成分が 4 隅すべてで**厳密に正**であることを要求する。
/// 画像座標系(y は下向き)では、この符号が「画面上で時計回り」=
/// tl → tr → br → bl の並びに対応する。0 を含めないことで、
/// 3 点が一直線に並ぶ退化四角形と、自己交差(蝶ネクタイ)形状も同時に弾く。
/// 逆順(bl, br, tr, tl 等)は符号が反転するので拒否される — 黙って鏡像の
/// 出力を返すより、順序の誤りとしてエラーにするほうが自己修復しやすい。
fn is_strictly_convex_in_order(q: &[[f64; 2]; 4]) -> bool {
    (0..4).all(|i| {
        let a = q[i];
        let b = q[(i + 1) % 4];
        let c = q[(i + 2) % 4];
        let cross = (b[0] - a[0]) * (c[1] - b[1]) - (b[1] - a[1]) * (c[0] - b[0]);
        cross > 0.0
    })
}

/// 戻り値: (出力画像, 警告, source→current 変換のこのステップ分)。
///
/// # 形式1: quad(4点指定)
///
/// 入力画像内の四角形 `[tl, tr, br, bl]`(連続座標)を、軸並行な長方形
/// `[0, W_out] x [0, H_out]` へ写す射影変換を求め、その長方形を出力キャンバスとする。
///
/// **出力寸法の規則(平均辺長の保存)**:
/// - `W_out = round((|tr - tl| + |br - bl|) / 2)` — 上辺と下辺の長さの相加平均
/// - `H_out = round((|bl - tl| + |br - tr|) / 2)` — 左辺と右辺の長さの相加平均
/// - いずれも 1 以上へクランプする
///
/// 台形の「奥の辺」は縮んで写っているので、上辺だけ・下辺だけを採ると出力が
/// 痩せる/太るが、相加平均を採ると補正後の見た目の面積が元の四角形とほぼ等しくなる。
/// 真の被写体アスペクト比は焦点距離が既知でなければ復元できない(4点だけからは
/// 不定)ため、「平均辺長」という**明示的で単純な規則**を採用し、正確な比率が要る
/// 用途は後段の `resize` / `crop` で指定してもらう方針とする。
///
/// # 形式2: キーストーン角
///
/// カメラを上に振ったことで生じる台形歪みを、光学中心を通る水平軸まわりの
/// 面回転として打ち消す。焦点距離は未知なので **`f = max(W, H)` 画素**と仮定する
/// (対角 FOV でおおむね 53〜67°、一般的な標準レンズ相当)。
///
/// 画像中心を原点とする連続座標 `X = x - W/2`, `Y = y - H/2` において、
/// `t_v = tan(vertical_degrees)`, `t_h = tan(horizontal_degrees)` として
///
/// ```text
///   X' = X / D,  Y' = Y / D,   D = 1 + (t_h * X + t_v * Y) / f
/// ```
///
/// 同次行列では `[[1,0,0], [0,1,0], [t_h/f, t_v/f, 1]]`(縦・横それぞれの
/// 基本行列の積に厳密に一致し、積の順序にも依らない)。
///
/// これは「各行を透視除算だけで伸縮させる」最小の射影であり、
/// 画像中心の倍率は厳密に 1 のままなので、`K R K^-1` から出る全体ズーム
/// (`1/cos θ` 系)を持ち込まない。したがって出力キャンバスは入力と同寸で足りる。
///
/// **符号**: `vertical_degrees > 0` は「カメラを上に振った(上辺が奥に倒れて
/// 狭く写っている)」ショットの補正 = **上辺を広げる**。実際 `Y < 0`(上側)では
/// `D < 1` で拡大、`Y > 0`(下側)では `D > 1` で縮小になる。
/// `horizontal_degrees` も対称に定義し、正なら **左辺を広げる**
/// (座標の小さい側を広げる、で縦横そろえてある)。
///
/// `|angle| <= 45°` と `f = max(W, H) >= H` から `|t * Y / f| <= 0.5` なので
/// `D >= 0.5 > 0`、すなわち画像内で地平線を跨がないことが保証される。
///
/// 出力キャンバスは入力と同寸。補正で画像外へ出た領域は `pad_color`(既定は白)で
/// 埋め、埋まった画素の割合を警告として返す。
///
/// # v2(f32 リニアライト)での補間
///
/// 補間(bicubic)は **線形光の f32 上で、アルファをプリマルチプライしてから** 行う。
/// 幾何写像そのもの(行列の組み立て・量子化・出力寸法の規則)は v1 と完全に同一で、
/// 変わったのは「何を混ぜるか」だけである。
pub fn apply(
    img: &LinearImage,
    quad: &Option<[[f64; 2]; 4]>,
    vertical_degrees: &Option<f64>,
    horizontal_degrees: &Option<f64>,
    pad_color: &Option<String>,
) -> Result<(LinearImage, Vec<String>, Transform)> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Err(AtxError::InvalidRecipe(
            "perspective: input image is empty".into(),
        ));
    }
    let pad = match pad_color {
        Some(c) => parse_hex_color(c).ok_or_else(|| {
            AtxError::InvalidRecipe(format!("perspective: invalid pad_color {c:?}"))
        })?,
        None => DEFAULT_PAD,
    };

    let (step, out_w, out_h) = match quad {
        Some(q) => quad_homography(*q, w, h),
        None => (
            keystone_homography(
                vertical_degrees.unwrap_or(0.0),
                horizontal_degrees.unwrap_or(0.0),
                w,
                h,
            ),
            w,
            h,
        ),
    };

    let inverse = step
        .inverse()
        .ok_or_else(|| AtxError::InvalidRecipe("perspective: the transform is singular".into()))?;

    let out = warp(img, &step, out_w, out_h, pad)?;

    let mut warnings = Vec::new();
    let padded = padding_ratio(&inverse, w, h, out_w, out_h);
    if padded > 0.0 {
        warnings.push(format!(
            "{:.1}% of the {out_w}x{out_h} output is padding \
             (areas with no source pixel behind them)",
            padded * 100.0
        ));
    }
    Ok((out, warnings, step))
}

/// 画像を単位長に正規化するスケール(1 単位 = `max(W, H)` 画素)。
/// 量子化の刻み 1e-6 が意味を持つ座標系を定めるためのもの。
fn norm_scale(w: u32, h: u32) -> f64 {
    w.max(h) as f64
}

/// 正規化座標で組んだ行列を画素座標へ戻す(`diag(s,s,1) * H * diag(1/s,1/s,1)`)。
/// 対角行列との合成なので要素ごとの掛け算/割り算だけで書ける。
fn denormalize(h: Transform, s: f64) -> Transform {
    let m = h.m;
    Transform::projective([
        [m[0][0], m[0][1], m[0][2] * s],
        [m[1][0], m[1][1], m[1][2] * s],
        [m[2][0] / s, m[2][1] / s, m[2][2]],
    ])
}

/// キーストーン角から画素座標の射影行列を組む(モデルは `apply` のドキュメント参照)。
fn keystone_homography(
    vertical_degrees: f64,
    horizontal_degrees: f64,
    w: u32,
    h: u32,
) -> Transform {
    // libm 由来の値はここで 1e-6 グリッドへ落とし、以降は四則演算のみ。
    let tv = quantize_1e6(vertical_degrees.to_radians().tan());
    let th = quantize_1e6(horizontal_degrees.to_radians().tan());

    // 正規化座標(1 単位 = f = max(W, H) 画素)では f が 1 になるので、
    // 最下行はそのまま tan になる。全係数が O(1) の状態で量子化する。
    let normalized =
        Transform::projective([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [th, tv, 1.0]]).quantized();

    let s = norm_scale(w, h);
    let centered = denormalize(normalized, s);
    // 画像中心を原点へ移す → 射影 → 中心を戻す(出力キャンバスは同寸)。
    let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
    Transform::translate(-cx, -cy)
        .then(centered)
        .then(Transform::translate(cx, cy))
}

/// quad → 軸並行長方形の射影行列と、出力寸法を求める。
fn quad_homography(q: [[f64; 2]; 4], w: u32, h: u32) -> (Transform, u32, u32) {
    let edge = |a: [f64; 2], b: [f64; 2]| -> f64 {
        // sqrt は libm 由来なので即量子化する(IEEE 的には sqrt は厳密丸めが
        // 要求されるが、規約どおり量子化を通しておく)。
        quantize_1e6(((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt())
    };
    let top = edge(q[0], q[1]);
    let bottom = edge(q[3], q[2]);
    let left = edge(q[0], q[3]);
    let right = edge(q[1], q[2]);
    let dim = |v: f64| -> u32 { v.round().clamp(1.0, u32::MAX as f64) as u32 };
    let out_w = dim((top + bottom) / 2.0);
    let out_h = dim((left + right) / 2.0);

    let s = norm_scale(w, h);
    let from = [
        [q[0][0] / s, q[0][1] / s],
        [q[1][0] / s, q[1][1] / s],
        [q[2][0] / s, q[2][1] / s],
        [q[3][0] / s, q[3][1] / s],
    ];
    let (ow, oh) = (out_w as f64 / s, out_h as f64 / s);
    let to = [[0.0, 0.0], [ow, 0.0], [ow, oh], [0.0, oh]];

    let normalized = match homography_from_points(&from, &to) {
        Some(t) => t.quantized(),
        // validate が厳密凸を保証しているので実際には到達しない。到達した場合は
        // 恒等変換にフォールバックし、`apply` 側の特異判定に任せる。
        None => Transform::IDENTITY,
    };
    (denormalize(normalized, s), out_w, out_h)
}

/// 4 点対応から射影変換を解く(DLT を 8 元 1 次方程式に落とし、部分ピボット付き
/// ガウス消去で解く)。`+ - * /` と比較しか使わないので完全に決定論的。
fn homography_from_points(from: &[[f64; 2]; 4], to: &[[f64; 2]; 4]) -> Option<Transform> {
    // 未知数 [h00 h01 h02 h10 h11 h12 h20 h21]、h22 = 1 に固定。
    let mut a = [[0.0f64; 9]; 8];
    for i in 0..4 {
        let (x, y) = (from[i][0], from[i][1]);
        let (u, v) = (to[i][0], to[i][1]);
        a[2 * i] = [x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y, u];
        a[2 * i + 1] = [0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y, v];
    }

    // 前進消去(部分ピボット選択)。
    for col in 0..8 {
        let mut pivot = col;
        for row in (col + 1)..8 {
            if a[row][col].abs() > a[pivot][col].abs() {
                pivot = row;
            }
        }
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        let pivot_row = a[col];
        let d = pivot_row[col];
        for row in a.iter_mut().skip(col + 1) {
            let factor = row[col] / d;
            if factor == 0.0 {
                continue;
            }
            for (k, v) in row.iter_mut().enumerate().skip(col) {
                *v -= factor * pivot_row[k];
            }
        }
    }
    // 後退代入。
    let mut x = [0.0f64; 8];
    for col in (0..8).rev() {
        let mut acc = a[col][8];
        for k in (col + 1)..8 {
            acc -= a[col][k] * x[k];
        }
        x[col] = acc / a[col][col];
    }
    if !x.iter().all(|v| v.is_finite()) {
        return None;
    }
    Some(Transform::projective([
        [x[0], x[1], x[2]],
        [x[3], x[4], x[5]],
        [x[6], x[7], 1.0],
    ]))
}

/// 射影 warp。`step` は**連続座標**の「入力 → 出力」写像。
///
/// `imageproc` の `warp_into` は **index 座標**で動くので、
/// `T(-0.5) ∘ step ∘ T(+0.5)` へ換算してから渡す(index `p` と連続 `u` は `u = p + 0.5`)。
/// 補間は bicubic、出力範囲外に落ちる画素は `pad` の定数境界で埋める。
/// `imageproc` 内部の座標計算は f32 だが、係数は f64 → 1e-6 グリッド量子化済みの
/// 値を丸めたものなので、プラットフォームによらず同じ f32 になる。
fn warp(
    img: &LinearImage,
    step: &Transform,
    out_w: u32,
    out_h: u32,
    pad: [u8; 4],
) -> Result<LinearImage> {
    let index_space = Transform::translate(0.5, 0.5)
        .then(*step)
        .then(Transform::translate(-0.5, -0.5));
    let m = index_space.m;
    #[rustfmt::skip]
    let coeffs = [
        m[0][0] as f32, m[0][1] as f32, m[0][2] as f32,
        m[1][0] as f32, m[1][1] as f32, m[1][2] as f32,
        m[2][0] as f32, m[2][1] as f32, m[2][2] as f32,
    ];
    let projection = Projection::from_matrix(coeffs).ok_or_else(|| {
        AtxError::InvalidRecipe("perspective: the transform is not invertible".into())
    })?;
    // 補間はプリマルチプライした線形光空間で行う。境界色も同じ空間へ持ち込む。
    let pad_linear = pad_to_linear(pad);
    let pad_premul = Rgba([
        pad_linear[0] * pad_linear[3],
        pad_linear[1] * pad_linear[3],
        pad_linear[2] * pad_linear[3],
        pad_linear[3],
    ]);
    let src = to_f32_image(&img.premultiplied());
    let mut warped: F32Image = F32Image::new(out_w, out_h);
    warp_into(
        &src,
        projection,
        Interpolation::Bicubic,
        Border::Constant(pad_premul),
        &mut warped,
    );
    let mut out = from_f32_image(&warped);
    out.unpremultiply();
    Ok(out)
}

/// 出力画素のうち「元画像の外に逆写像されるもの」= パディングの割合(0.0..=1.0)。
///
/// 判定は f64 の逆行列で行う(warp 本体は f32 で逆写像するので境界 1 画素ぶんの
/// ずれはありうるが、これは警告用の統計値なので実用上問題ない)。
fn padding_ratio(inverse: &Transform, in_w: u32, in_h: u32, out_w: u32, out_h: u32) -> f64 {
    let (fw, fh) = (in_w as f64, in_h as f64);
    let mut padded: u64 = 0;
    for py in 0..out_h {
        for px in 0..out_w {
            let (x, y) = inverse.map(px as f64 + 0.5, py as f64 + 0.5);
            if !(x.is_finite() && y.is_finite()) || x < 0.0 || y < 0.0 || x >= fw || y >= fh {
                padded += 1;
            }
        }
    }
    padded as f64 / (out_w as f64 * out_h as f64)
}
