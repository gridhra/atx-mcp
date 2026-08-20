//! 帯域限定の高分解能 Hough 変換。
//!
//! `imageproc::hough::detect_lines` は角度ビンが 1° 固定であり、要求精度
//! (±0.3°)を満たせない。ここでは同じ投票法を
//! - 角度探索範囲を「ほぼ水平」「ほぼ垂直」の 2 バンドに限定
//! - 角度ビン幅を 0.1°(`ANGLE_STEP_DEG`)に細分
//!
//! した形で自前実装する。エッジ画素の勾配方向でどちらのバンドに投票するかを
//! 決めるため、計算量は 1 画素あたり片方のバンド分のみ。
//!
//! 座標系は画像座標(x 右・y 下)。極座標表現は imageproc と同じ
//! `r = x*cos(theta) + y*sin(theta)`(ただし原点は画像中心)を用いる。
//! - 水平バンド: theta = 90° + phi
//! - 垂直バンド: theta = phi
//!
//! いずれも `phi` は「画面上で時計回りに phi 度傾いている」ことを意味する
//! (詳細は [`crate`] のドキュメントを参照)。

use image::GrayImage;
use imageproc::gradients::{horizontal_sobel, vertical_sobel};

/// 角度ビン幅(度)。
pub const ANGLE_STEP_DEG: f64 = 0.1;

/// 線の所属バンド。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// ほぼ水平な線(水平線・地平線・軒など)。
    Horizontal,
    /// ほぼ垂直な線(柱・建物の縦エッジなど)。
    Vertical,
}

/// 検出された直線 1 本。
#[derive(Debug, Clone, Copy)]
pub struct DetectedLine {
    /// 画面上の時計回り傾き(度)。0 = 完全に水平 / 垂直。
    pub phi_deg: f64,
    /// 投票数(≒ 画像内での線長 [px])。
    pub votes: f64,
    /// 所属バンド。
    pub family: Family,
}

/// Hough 用の入力エッジ点(画像中心を原点とする座標 + 所属バンド)。
struct EdgePoint {
    dx: f32,
    dy: f32,
    family: Family,
}

/// Canny 出力と元グレースケールから直線群を検出する。
///
/// - `edges`: Canny の二値画像(非ゼロ = エッジ)
/// - `gray`: 勾配方向の算出元(Canny と同じ 1.4 の Gaussian をかけた画像)
/// - `max_abs_angle`: 探索する最大傾き(度)
pub fn detect_lines_fine(
    edges: &GrayImage,
    blurred: &GrayImage,
    max_abs_angle: f64,
) -> Vec<DetectedLine> {
    let (w, h) = edges.dimensions();
    if w < 16 || h < 16 {
        return Vec::new();
    }

    let gx = horizontal_sobel(blurred);
    let gy = vertical_sobel(blurred);

    // 勾配方向が軸から離れすぎている画素は捨てる(斜め構造・ノイズ除去)。
    let slack = (max_abs_angle + 6.0).to_radians().tan() as f32;

    let cx = (w - 1) as f32 / 2.0;
    let cy = (h - 1) as f32 / 2.0;

    let mut points: Vec<EdgePoint> = Vec::new();
    for y in 0..h {
        for x in 0..w {
            if edges.get_pixel(x, y)[0] == 0 {
                continue;
            }
            let ex = gx.get_pixel(x, y)[0] as f32;
            let ey = gy.get_pixel(x, y)[0] as f32;
            if ex == 0.0 && ey == 0.0 {
                continue;
            }
            // 勾配が縦向き(|ey| > |ex|)ならエッジ自体はほぼ水平。
            let family = if ey.abs() > ex.abs() {
                // 線方向 = (-ey, ex) を水平基準に。傾き = ex / -ey。
                if (ex / ey).abs() > slack {
                    continue;
                }
                Family::Horizontal
            } else {
                if (ey / ex).abs() > slack {
                    continue;
                }
                Family::Vertical
            };
            points.push(EdgePoint {
                dx: x as f32 - cx,
                dy: y as f32 - cy,
                family,
            });
        }
    }

    if points.is_empty() {
        return Vec::new();
    }

    let n_angles = (2.0 * max_abs_angle / ANGLE_STEP_DEG).round() as usize + 1;
    let phis: Vec<f64> = (0..n_angles)
        .map(|i| -max_abs_angle + i as f64 * ANGLE_STEP_DEG)
        .collect();
    let trig: Vec<(f32, f32)> = phis
        .iter()
        .map(|p| {
            let (s, c) = p.to_radians().sin_cos();
            (s as f32, c as f32)
        })
        .collect();

    let rmax = (((w * w + h * h) as f64).sqrt() / 2.0).ceil() as i32 + 2;
    let r_bins = (2 * rmax + 1) as usize;

    let mut acc_h = vec![0u32; n_angles * r_bins];
    let mut acc_v = vec![0u32; n_angles * r_bins];
    let mut n_h = 0usize;
    let mut n_v = 0usize;

    for p in &points {
        let (acc, n) = match p.family {
            Family::Horizontal => (&mut acc_h, &mut n_h),
            Family::Vertical => (&mut acc_v, &mut n_v),
        };
        *n += 1;
        for (ai, &(s, c)) in trig.iter().enumerate() {
            let r = match p.family {
                Family::Horizontal => -p.dx * s + p.dy * c,
                Family::Vertical => p.dx * c + p.dy * s,
            };
            let idx = (r.round() as i32 + rmax) as usize;
            if idx < r_bins {
                acc[ai * r_bins + idx] += 1;
            }
        }
    }

    let long_edge = w.max(h) as f64;
    let mut lines = Vec::new();
    for (acc, n, family) in [
        (&acc_h, n_h, Family::Horizontal),
        (&acc_v, n_v, Family::Vertical),
    ] {
        if n == 0 {
            continue;
        }
        // 閾値: 「画像長辺の 12% 以上の長さ」かつ「一様分布時の期待値の 5 倍以上」。
        // 後者がノイズ画像での偽検出を抑える。
        let expected = n as f64 / r_bins as f64;
        let threshold = (0.12 * long_edge).max(5.0 * expected).max(24.0);
        lines.extend(extract_peaks(
            acc, n_angles, r_bins, threshold, &phis, family,
        ));
    }

    // 決定論的な順序(votes 降順 → phi 昇順)。
    lines.sort_by(|a, b| {
        b.votes
            .partial_cmp(&a.votes)
            .unwrap()
            .then(a.phi_deg.partial_cmp(&b.phi_deg).unwrap())
    });
    lines.truncate(200);
    lines
}

/// アキュムレータから非極大抑制付きでピークを取り出す。
fn extract_peaks(
    acc: &[u32],
    n_angles: usize,
    r_bins: usize,
    threshold: f64,
    phis: &[f64],
    family: Family,
) -> Vec<DetectedLine> {
    // 抑制窓: r 方向 ±6px、角度方向 ±0.5°。
    const R_RAD: usize = 6;
    let a_rad: usize = (0.5 / ANGLE_STEP_DEG).round() as usize;

    let mut out = Vec::new();
    for ai in 0..n_angles {
        for ri in 0..r_bins {
            let v = acc[ai * r_bins + ri];
            if (v as f64) < threshold {
                continue;
            }
            let mut is_max = true;
            'outer: for aj in ai.saturating_sub(a_rad)..(ai + a_rad + 1).min(n_angles) {
                for rj in ri.saturating_sub(R_RAD)..(ri + R_RAD + 1).min(r_bins) {
                    if aj == ai && rj == ri {
                        continue;
                    }
                    let u = acc[aj * r_bins + rj];
                    // 同値の場合は先に現れた方(角度小 → r 小)を採用。
                    if u > v || (u == v && (aj, rj) < (ai, ri)) {
                        is_max = false;
                        break 'outer;
                    }
                }
            }
            if is_max {
                out.push(DetectedLine {
                    phi_deg: refine_angle(acc, n_angles, r_bins, ai, ri, phis),
                    votes: v as f64,
                    family,
                });
            }
        }
    }
    out
}

/// 角度方向のパラボラ補間でビン中心よりも細かい角度を得る。
fn refine_angle(
    acc: &[u32],
    n_angles: usize,
    r_bins: usize,
    ai: usize,
    ri: usize,
    phis: &[f64],
) -> f64 {
    if ai == 0 || ai + 1 >= n_angles {
        return phis[ai];
    }
    let l = acc[(ai - 1) * r_bins + ri] as f64;
    let c = acc[ai * r_bins + ri] as f64;
    let r = acc[(ai + 1) * r_bins + ri] as f64;
    let denom = l - 2.0 * c + r;
    if denom >= 0.0 {
        return phis[ai];
    }
    let delta = (0.5 * (l - r) / denom).clamp(-0.5, 0.5);
    phis[ai] + delta * ANGLE_STEP_DEG
}
