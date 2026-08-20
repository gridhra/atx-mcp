//! 角度ヒストグラム・ピーク選択・信頼度・クロップ損失の算出。

use crate::hough::{DetectedLine, Family, ANGLE_STEP_DEG};

/// ピーク近傍とみなす角度半径(度)。
pub const PEAK_RADIUS_DEG: f64 = 0.8;
/// ヒストグラム平滑化の標準偏差(度)。
const SMOOTH_SIGMA_DEG: f64 = 0.25;

/// 平滑化済み角度ヒストグラム。
pub struct AngleHistogram {
    /// bin 中心角(度、phi = 画面上の時計回り傾き)。
    pub centers: Vec<f64>,
    /// bin 値(線長重みの総和)。
    pub values: Vec<f64>,
}

impl AngleHistogram {
    /// 検出線から重み付きヒストグラムを構築する(Gaussian ソフト投票)。
    pub fn build(lines: &[DetectedLine], max_abs_angle: f64) -> Self {
        let n = (2.0 * max_abs_angle / ANGLE_STEP_DEG).round() as usize + 1;
        let centers: Vec<f64> = (0..n)
            .map(|i| -max_abs_angle + i as f64 * ANGLE_STEP_DEG)
            .collect();
        let mut values = vec![0.0f64; n];
        let sigma_bins = SMOOTH_SIGMA_DEG / ANGLE_STEP_DEG;
        let radius = (3.0 * sigma_bins).ceil() as i64;
        for line in lines {
            let pos = (line.phi_deg + max_abs_angle) / ANGLE_STEP_DEG;
            let center = pos.round() as i64;
            for k in (center - radius)..=(center + radius) {
                if k < 0 || k as usize >= n {
                    continue;
                }
                let d = (k as f64 - pos) / sigma_bins;
                values[k as usize] += line.votes * (-0.5 * d * d).exp();
            }
        }
        Self { centers, values }
    }

    /// 合計質量。
    pub fn total(&self) -> f64 {
        self.values.iter().sum()
    }

    /// ピーク近傍の質量。
    pub fn mass_near(&self, phi: f64) -> f64 {
        self.centers
            .iter()
            .zip(&self.values)
            .filter(|(c, _)| (**c - phi).abs() <= PEAK_RADIUS_DEG)
            .map(|(_, v)| *v)
            .sum()
    }

    /// 局所最大を強い順に返す。`min_separation_deg` 未満の間隔のピークは捨てる。
    pub fn peaks(&self, min_separation_deg: f64, limit: usize) -> Vec<f64> {
        let n = self.values.len();
        let mut cands: Vec<(f64, f64)> = Vec::new(); // (value, refined phi)
        for i in 0..n {
            let v = self.values[i];
            if v <= 0.0 {
                continue;
            }
            let l = if i == 0 { 0.0 } else { self.values[i - 1] };
            let r = if i + 1 >= n { 0.0 } else { self.values[i + 1] };
            // 同値の平坦部では左端のみを採用(決定論)。
            if v < l || v < r || (v == l) {
                continue;
            }
            cands.push((v, parabolic(l, v, r, self.centers[i])));
        }
        cands.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap()
                .then(a.1.partial_cmp(&b.1).unwrap())
        });

        let mut out: Vec<f64> = Vec::new();
        for (_, phi) in cands {
            if out.iter().any(|p| (p - phi).abs() < min_separation_deg) {
                continue;
            }
            out.push(phi);
            if out.len() >= limit {
                break;
            }
        }
        out
    }
}

/// 3 点パラボラ補間によるサブビン精度のピーク位置。
fn parabolic(l: f64, c: f64, r: f64, center: f64) -> f64 {
    let denom = l - 2.0 * c + r;
    if denom >= 0.0 {
        return center;
    }
    let delta = (0.5 * (l - r) / denom).clamp(-0.5, 0.5);
    center + delta * ANGLE_STEP_DEG
}

/// ピーク近傍の線群から得られる推定量。
pub struct PeakSupport {
    /// 投票数重み付き平均の phi(度)。
    pub phi_deg: f64,
    /// 支持線の本数。
    pub line_count: usize,
    /// 水平バンドの推定 phi(支持線があれば)。
    pub phi_h: Option<f64>,
    /// 垂直バンドの推定 phi(支持線があれば)。
    pub phi_v: Option<f64>,
}

/// ピーク近傍の支持線を集計する。
pub fn support_for(lines: &[DetectedLine], peak_phi: f64) -> PeakSupport {
    let mut sum_w = 0.0;
    let mut sum_wp = 0.0;
    let mut count = 0usize;
    let mut fam = [(0.0f64, 0.0f64); 2]; // (weight, weight*phi) for [H, V]
    for line in lines {
        if (line.phi_deg - peak_phi).abs() > PEAK_RADIUS_DEG {
            continue;
        }
        count += 1;
        sum_w += line.votes;
        sum_wp += line.votes * line.phi_deg;
        let i = match line.family {
            Family::Horizontal => 0,
            Family::Vertical => 1,
        };
        fam[i].0 += line.votes;
        fam[i].1 += line.votes * line.phi_deg;
    }
    let mean = |(w, wp): (f64, f64)| if w > 0.0 { Some(wp / w) } else { None };
    PeakSupport {
        phi_deg: if sum_w > 0.0 {
            sum_wp / sum_w
        } else {
            peak_phi
        },
        line_count: count,
        phi_h: mean(fam[0]),
        phi_v: mean(fam[1]),
    }
}

/// 信頼度を 0..=1 で算出する。
///
/// - `dominance`: ピーク質量 / 全質量
/// - 支持線本数: 6 本で飽和
/// - 水平・垂直バンドの一致度: 差 1.0° で 0
///
/// さらに、支持線が `min_lines` 未満の場合は線形にゲートする。
/// 単独の偶発的な線は dominance が自明に 1 になってしまうため
/// (ノイズ画像での偽陽性)、証拠量そのもので割り引く必要がある。
pub fn confidence(dominance: f64, support: &PeakSupport, min_lines: usize) -> f64 {
    let support_score = (support.line_count as f64 / 6.0).min(1.0);
    let agreement = match (support.phi_h, support.phi_v) {
        (Some(h), Some(v)) => (1.0 - (h - v).abs() / 1.0).clamp(0.0, 1.0),
        // 片方のバンドしか根拠がない場合は中庸の値。
        _ => 0.65,
    };
    let gate = (support.line_count as f64 / min_lines.max(1) as f64).min(1.0);
    let base = 0.40 * dominance.clamp(0.0, 1.0) + 0.35 * support_score + 0.25 * agreement;
    (base * gate).clamp(0.0, 1.0)
}

/// w×h の矩形を angle 度回転したときの、内接する軸平行最大矩形による画素損失率(%)。
pub fn inscribed_crop_loss_percent(w: u32, h: u32, angle_degrees: f64) -> f64 {
    if w == 0 || h == 0 {
        return 0.0;
    }
    let w = w as f64;
    let h = h as f64;
    let a = angle_degrees.to_radians().abs() % std::f64::consts::PI;
    let a = if a > std::f64::consts::FRAC_PI_2 {
        std::f64::consts::PI - a
    } else {
        a
    };
    let (sin_a, cos_a) = a.sin_cos();
    let (long, short) = if w >= h { (w, h) } else { (h, w) };
    let (wr, hr) = if short <= 2.0 * sin_a * cos_a * long || (sin_a - cos_a).abs() < 1e-10 {
        let x = 0.5 * short;
        if w >= h {
            (x / sin_a.max(1e-12), x / cos_a.max(1e-12))
        } else {
            (x / cos_a.max(1e-12), x / sin_a.max(1e-12))
        }
    } else {
        let cos_2a = cos_a * cos_a - sin_a * sin_a;
        (
            (w * cos_a - h * sin_a) / cos_2a,
            (h * cos_a - w * sin_a) / cos_2a,
        )
    };
    let kept = (wr.max(0.0) * hr.max(0.0)) / (w * h);
    (100.0 * (1.0 - kept)).clamp(0.0, 100.0)
}
