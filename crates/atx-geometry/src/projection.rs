//! 投影プロファイル法(projection profile)による傾き推定。
//!
//! Hough が「長い直線」を必要とするのに対し、この手法は**短く途切れたエッジの
//! 集合**でも安定する。実写(社殿の屋根・板塀・階段など、短いエッジが多数)で
//! Hough が 0.1° 弱ずれる問題に対する補強として導入した。
//!
//! # 原理
//!
//! 1. Sobel 勾配から「ほぼ水平なエッジ」(|gy| 優勢)と「ほぼ垂直なエッジ」
//!    (|gx| 優勢)を分離する。
//! 2. 候補角 phi(画面上の時計回り傾き)に対し、水平族は
//!    `t = y - x * tan(phi)`、垂直族は `t = x + y * tan(phi)` で
//!    シアーした座標を 1px ビンへ**バイリニアに撒く**(重み = 勾配強度)。
//!    phi が真の傾きと一致したとき、同じ直線上の画素がすべて同じビンへ集まる。
//! 3. スコア = プロファイルの二乗和 / 総質量^2(= 正規化した分散に単調)。
//!    ビン数を phi によらず固定しているため、総質量が保存され角度間で比較可能。
//! 4. ±max_abs_angle を 0.1° グリッドで粗探索し、最良点の周囲 ±0.3° を
//!    固定点数サンプリング + 重心で細分する(乱数・スレッドなし = 完全に決定論的)。
//!    ピークは「±0.5px 相当」の幅で平坦になるため、argmax ではなく重心を使う。

use image::GrayImage;
use imageproc::gradients::{horizontal_sobel, vertical_sobel};

use crate::hough::Family;

/// 粗探索のステップ(度)。
pub const SCAN_STEP_DEG: f64 = 0.1;
/// スコア曲線の最大点数(出力サイズの上限)。
pub const CURVE_MAX_POINTS: usize = 300;
/// 投影に使うエッジ点数の上限(実行時間の上限を与える)。
const MAX_POINTS: usize = 40_000;
/// 候補画素のうち強度上位から残す割合(解像度に依らない選別のため)。
const KEEP_FRACTION: f64 = 0.15;
/// 勾配強度の下限(これ未満はノイズとして捨てる)。
const MIN_MAGNITUDE: f64 = 12.0;
/// 最強エッジ(99.5 パーセンタイル)に対する相対下限。
const STRONG_EDGE_RATIO: f64 = 0.15;
/// 族として成立するのに必要な最小エッジ点数。
const MIN_FAMILY_POINTS: usize = 64;
/// 細分サンプリングの点数(固定 = 決定論)。
/// 細分に使う窓の半径(度)。粗探索ピークの左右対称性を活かすため広めに取る。
const REFINE_RADIUS_DEG: f64 = 0.3;
const REFINE_SAMPLES: usize = 61;

/// 投影に使うエッジ点(画像中心を原点とする座標 + 重み)。
#[derive(Clone, Copy)]
struct EdgePoint {
    x: f32,
    y: f32,
    w: f32,
}

/// 1 つの族(水平 / 垂直)の投影プロファイル推定。
#[derive(Debug, Clone)]
pub struct FamilyEstimate {
    /// 画面上の時計回り傾き(度)。
    pub phi_deg: f64,
    /// ピークの鋭さから求めた 0..=1 の信頼度。
    pub sharpness: f64,
    /// この族が持つエッジ質量(絶対値)。
    pub mass: f64,
}

/// 投影プロファイル法の全体結果。
#[derive(Debug, Clone)]
pub struct ProjectionEstimate {
    /// 水平族(ほぼ水平なエッジ)の推定。
    pub horizontal: Option<FamilyEstimate>,
    /// 垂直族(ほぼ垂直なエッジ)の推定。
    pub vertical: Option<FamilyEstimate>,
    /// 両族を合成した目的関数の粗探索結果 `(phi, 0..=1 に正規化したスコア)`。
    /// 粗探索グリッド(0.1° 刻み)の全点。間引きと Hough 証拠との合成は呼び出し側。
    pub curve: Vec<(f64, f64)>,
    /// 合成目的関数の細分済みピーク(画面上の時計回り傾き、度)。
    pub fused_phi_deg: Option<f64>,
    /// 合成ピークの鋭さ(0..=1)。
    pub fused_sharpness: f64,
}

impl Default for ProjectionEstimate {
    /// 推定が 1 つも得られなかった空の結果。
    fn default() -> Self {
        Self {
            horizontal: None,
            vertical: None,
            curve: Vec::new(),
            fused_phi_deg: None,
            fused_sharpness: 0.0,
        }
    }
}

impl ProjectionEstimate {
    /// 水平族の質量比(0..=1)。両族が空なら 0。
    pub fn horizontal_support(&self) -> f64 {
        self.support(&self.horizontal)
    }

    /// 垂直族の質量比(0..=1)。
    pub fn vertical_support(&self) -> f64 {
        self.support(&self.vertical)
    }

    fn support(&self, e: &Option<FamilyEstimate>) -> f64 {
        let total = self.horizontal.as_ref().map_or(0.0, |f| f.mass)
            + self.vertical.as_ref().map_or(0.0, |f| f.mass);
        match (e, total > 0.0) {
            (Some(f), true) => (f.mass / total).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }
}

/// 投影プロファイル法の探索器。角度を与えるとスコアを返せる状態を保持する。
pub struct ProjectionSearch {
    horizontal: FamilyField,
    vertical: FamilyField,
    max_abs_angle: f64,
}

impl ProjectionSearch {
    /// ぼかし済みグレースケールからエッジ点を抽出して探索器を作る。
    pub fn new(blurred: &GrayImage, max_abs_angle: f64) -> Self {
        let (h_pts, v_pts) = extract_points(blurred, max_abs_angle);
        let (w, h) = blurred.dimensions();
        let tan_max = max_abs_angle.to_radians().tan();
        let half_w = (w as f64 - 1.0) / 2.0;
        let half_h = (h as f64 - 1.0) / 2.0;
        Self {
            horizontal: FamilyField::new(h_pts, half_h + tan_max * half_w),
            vertical: FamilyField::new(v_pts, half_w + tan_max * half_h),
            max_abs_angle,
        }
    }

    /// エッジ抽出 + 粗探索 + 細分をまとめて実行する。
    ///
    /// 返り値の探索器は正規化レンジが確定済みなので、
    /// [`ProjectionSearch::refine_near`] で別手法の候補角を細分できる。
    pub fn run(blurred: &GrayImage, max_abs_angle: f64) -> (Self, ProjectionEstimate) {
        let mut search = Self::new(blurred, max_abs_angle);
        let est = search.estimate();
        (search, est)
    }

    /// 粗探索 + 重心細分で全体を推定する。
    pub fn estimate(&mut self) -> ProjectionEstimate {
        let grid = angle_grid(self.max_abs_angle);
        if grid.len() < 3 {
            return ProjectionEstimate::default();
        }

        let h = self.estimate_family(Family::Horizontal, &grid);
        let v = self.estimate_family(Family::Vertical, &grid);
        if h.is_none() && v.is_none() {
            return ProjectionEstimate::default();
        }

        // 合成目的関数: 各族の正規化スコアを「質量 × ピークの鋭さ」で重み付けする。
        // 質量だけで重み付けすると、パースで扇状に開いた柱群(ピークが鈍い)が
        // 数の力で答えを引っ張ってしまう。
        let (wh, wv) = weights(h.as_ref().map(|(e, _)| e), v.as_ref().map(|(e, _)| e));

        let mut fused: Vec<f64> = vec![0.0; grid.len()];
        for (e, w) in [(&h, wh), (&v, wv)] {
            if let Some((_, norm)) = e {
                for (i, n) in norm.iter().enumerate() {
                    fused[i] += w * n;
                }
            }
        }

        let (best_i, _) = arg_max(&fused);
        let fused_phi = self.refine(&grid, best_i, wh, wv);
        let fused_sharpness = sharpness(&fused);
        let norm = normalize(&fused);
        let curve: Vec<(f64, f64)> = grid.iter().copied().zip(norm).collect();

        ProjectionEstimate {
            horizontal: h.map(|(e, _)| e),
            vertical: v.map(|(e, _)| e),
            curve,
            fused_phi_deg: Some(fused_phi),
            fused_sharpness,
        }
    }

    /// Hough などが与えた候補角の周囲 ±`radius_deg` を投影プロファイルで細分する。
    ///
    /// 合成目的関数(粗探索で得た族ごとの正規化係数を使う)を最大化する。
    pub fn refine_near(&self, phi_deg: f64, radius_deg: f64, est: &ProjectionEstimate) -> f64 {
        let (wh, wv) = family_weights(est);
        let lo = (phi_deg - radius_deg).max(-self.max_abs_angle);
        let hi = (phi_deg + radius_deg).min(self.max_abs_angle);
        if hi <= lo {
            return phi_deg;
        }
        let f = |p: f64| self.fused_raw(p, wh, wv);
        // 重心細分は「窓がピークの中心にある」ことを前提にするので、まず粗探索で
        // 窓の中心を決める(与えられた候補角そのものを中心にすると、ずれた分だけ
        // 重心も引きずられる)。
        let n = ((hi - lo) / SCAN_STEP_DEG).round() as usize + 1;
        let coarse: Vec<f64> = (0..n).map(|i| f(lo + i as f64 * SCAN_STEP_DEG)).collect();
        let (best_i, _) = arg_max(&coarse);
        let center = lo + best_i as f64 * SCAN_STEP_DEG;
        refine_max(
            (center - REFINE_RADIUS_DEG).max(-self.max_abs_angle),
            (center + REFINE_RADIUS_DEG).min(self.max_abs_angle),
            f,
        )
    }

    /// 指定した角度の近傍 ±`radius_deg` に限った族単体の推定。
    ///
    /// 族の**大域**ピークをそのまま返すと、画面の隅にある別構造(隣の建物の
    /// 庇、遠近で扇状に開いた敷石の目地など)が勝ってしまい、解像度によって
    /// 答えが飛ぶ。「いま推奨している角度の近傍で、この族は何と言っているか」
    /// の方が診断として有用で、かつ安定する。
    pub fn family_near(
        &self,
        family: Family,
        phi_deg: f64,
        radius_deg: f64,
    ) -> Option<FamilyEstimate> {
        let field = match family {
            Family::Horizontal => &self.horizontal,
            Family::Vertical => &self.vertical,
        };
        if field.points.len() < MIN_FAMILY_POINTS {
            return None;
        }
        let lo = (phi_deg - radius_deg).max(-self.max_abs_angle);
        let hi = (phi_deg + radius_deg).min(self.max_abs_angle);
        if hi <= lo {
            return None;
        }
        let n = ((hi - lo) / SCAN_STEP_DEG).round() as usize + 1;
        let coarse: Vec<f64> = (0..n)
            .map(|i| field.score(family, lo + i as f64 * SCAN_STEP_DEG))
            .collect();
        // 窓の内部にある極大だけを候補にする。端が最大の場合は「窓の外にもっと
        // 強い別構造がある」ということなので、この族はこの近傍で何も言えない。
        let mut best: Option<(usize, f64)> = None;
        for i in 1..n.saturating_sub(1) {
            let v = coarse[i];
            if v >= coarse[i - 1] && v >= coarse[i + 1] && best.is_none_or(|(_, b)| v > b) {
                best = Some((i, v));
            }
        }
        let (best_i, _) = best?;
        let center = lo + best_i as f64 * SCAN_STEP_DEG;
        let phi = refine_max(
            (center - REFINE_RADIUS_DEG).max(lo),
            (center + REFINE_RADIUS_DEG).min(hi),
            |p| field.score(family, p),
        );
        // 局所ピークの高さ(大域レンジ内での位置)× 大域的なピークの鋭さ。
        let local = field.normalized_score(family, phi).clamp(0.0, 1.0);
        Some(FamilyEstimate {
            phi_deg: phi,
            sharpness: local * field.sharpness,
            mass: field.mass,
        })
    }

    /// 族ごとの重み付き生スコア和(正規化係数は単調変換なのでピーク位置に影響しない
    /// が、族間の比較のためスケールを質量で揃える)。
    fn fused_raw(&self, phi: f64, wh: f64, wv: f64) -> f64 {
        let mut s = 0.0;
        if wh > 0.0 {
            s += wh * self.horizontal.normalized_score(Family::Horizontal, phi);
        }
        if wv > 0.0 {
            s += wv * self.vertical.normalized_score(Family::Vertical, phi);
        }
        s
    }

    fn refine(&self, grid: &[f64], best_i: usize, wh: f64, wv: f64) -> f64 {
        let step = REFINE_RADIUS_DEG;
        let lo = (grid[best_i] - step).max(-self.max_abs_angle);
        let hi = (grid[best_i] + step).min(self.max_abs_angle);
        if hi <= lo {
            return grid[best_i];
        }
        refine_max(lo, hi, |p| self.fused_raw(p, wh, wv))
    }

    /// 族単体の粗探索 + 細分。有効でなければ None。
    ///
    /// 併せて正規化レンジ(粗探索の min/max)を族に記録する。
    fn estimate_family(
        &mut self,
        family: Family,
        grid: &[f64],
    ) -> Option<(FamilyEstimate, Vec<f64>)> {
        let field = match family {
            Family::Horizontal => &mut self.horizontal,
            Family::Vertical => &mut self.vertical,
        };
        if field.points.len() < MIN_FAMILY_POINTS {
            return None;
        }
        let scores: Vec<f64> = grid.iter().map(|&p| field.score(family, p)).collect();
        let (best_i, max) = arg_max(&scores);
        let min = scores.iter().copied().fold(f64::INFINITY, f64::min);
        field.range = (min, max);
        let sharp = sharpness(&scores);
        field.sharpness = sharp;

        let lo = (grid[best_i] - REFINE_RADIUS_DEG).max(grid[0]);
        let hi = (grid[best_i] + REFINE_RADIUS_DEG).min(grid[grid.len() - 1]);
        let phi = if hi > lo {
            refine_max(lo, hi, |p| field.score(family, p))
        } else {
            grid[best_i]
        };

        let norm = normalize(&scores);
        Some((
            FamilyEstimate {
                phi_deg: phi,
                sharpness: sharp,
                mass: field.mass,
            },
            norm,
        ))
    }
}

/// 合成目的関数の族重み(質量 × ピークの鋭さ、和が 1)。
fn family_weights(est: &ProjectionEstimate) -> (f64, f64) {
    weights(est.horizontal.as_ref(), est.vertical.as_ref())
}

/// 族の重み。鋭さが 0 の族は完全には落とさない(下限 0.1)。
fn weights(h: Option<&FamilyEstimate>, v: Option<&FamilyEstimate>) -> (f64, f64) {
    let score = |e: Option<&FamilyEstimate>| {
        e.map_or(0.0, |f| f.mass * f.sharpness.clamp(0.0, 1.0).max(0.1))
    };
    let (a, b) = (score(h), score(v));
    let total = a + b;
    if total > 0.0 {
        (a / total, b / total)
    } else {
        (0.0, 0.0)
    }
}

/// 1 つの族のエッジ点集合とビン配置。
struct FamilyField {
    points: Vec<EdgePoint>,
    mass: f64,
    /// ビン中心のオフセット(t + offset がビン座標)。
    offset: f64,
    n_bins: usize,
    /// 粗探索で得た正規化係数 `(min, max)`。
    range: (f64, f64),
    /// 粗探索で得たピークの鋭さ(0..=1)。
    sharpness: f64,
}

impl FamilyField {
    fn new(points: Vec<EdgePoint>, half_range: f64) -> Self {
        let mass = points.iter().map(|p| p.w as f64).sum();
        let offset = half_range.ceil() + 2.0;
        let n_bins = (2.0 * offset).ceil() as usize + 1;
        Self {
            points,
            mass,
            offset,
            n_bins,
            range: (0.0, 1.0),
            sharpness: 0.0,
        }
    }

    /// プロファイルの二乗和 / 総質量^2。
    fn score(&self, family: Family, phi_deg: f64) -> f64 {
        if self.points.is_empty() {
            return 0.0;
        }
        let tan = phi_deg.to_radians().tan() as f32;
        let mut bins = vec![0.0f64; self.n_bins];
        let off = self.offset as f32;
        for p in &self.points {
            let t = match family {
                Family::Horizontal => p.y - p.x * tan,
                Family::Vertical => p.x + p.y * tan,
            } + off;
            if t < 0.0 {
                continue;
            }
            let i = t as usize;
            if i + 1 >= self.n_bins {
                continue;
            }
            let frac = (t - i as f32) as f64;
            let w = p.w as f64;
            bins[i] += w * (1.0 - frac);
            bins[i + 1] += w * frac;
        }
        // ビン格子との「位相」に対する感度を落とすため軽く平滑化する。
        // これがないと、画像高さ 1px 分のシアー(≒0.1°)ごとにスコアが
        // 波打ち、真のピークの隣に偽ピークが立つ(サブビンのエイリアス)。
        smooth3(&mut bins);
        smooth3(&mut bins);
        let total: f64 = bins.iter().sum();
        if total <= 0.0 {
            return 0.0;
        }
        let sq: f64 = bins.iter().map(|v| v * v).sum();
        sq / (total * total)
    }

    /// 粗探索レンジで 0..=1 に正規化したスコア。
    fn normalized_score(&self, family: Family, phi_deg: f64) -> f64 {
        let (lo, hi) = self.range;
        let s = self.score(family, phi_deg);
        if hi > lo {
            (s - lo) / (hi - lo)
        } else {
            s
        }
    }
}

/// [1,2,1]/4 の 3 タップ平滑化(端はゼロ拡張)。
fn smooth3(v: &mut [f64]) {
    let n = v.len();
    if n < 3 {
        return;
    }
    let mut prev = 0.0;
    for i in 0..n {
        let cur = v[i];
        let next = if i + 1 < n { v[i + 1] } else { 0.0 };
        v[i] = 0.25 * prev + 0.5 * cur + 0.25 * next;
        prev = cur;
    }
}

/// 粗探索グリッド(0.1° 刻み)。
fn angle_grid(max_abs_angle: f64) -> Vec<f64> {
    let n = (2.0 * max_abs_angle / SCAN_STEP_DEG).round() as usize + 1;
    (0..n)
        .map(|i| -max_abs_angle + i as f64 * SCAN_STEP_DEG)
        .collect()
}

/// 最大値の位置(同値は先に現れた方 = 決定論)。
fn arg_max(v: &[f64]) -> (usize, f64) {
    let mut bi = 0usize;
    let mut bv = f64::NEG_INFINITY;
    for (i, &x) in v.iter().enumerate() {
        if x > bv {
            bv = x;
            bi = i;
        }
    }
    (bi, bv)
}

/// 0..=1 に線形正規化する。
fn normalize(v: &[f64]) -> Vec<f64> {
    let (_, max) = arg_max(v);
    let min = v.iter().copied().fold(f64::INFINITY, f64::min);
    if max > min {
        v.iter().map(|x| (x - min) / (max - min)).collect()
    } else {
        vec![0.0; v.len()]
    }
}

/// ピークの鋭さ(0..=1): 中央値がピークからどれだけ低いか。
///
/// 直線が揃った画像では、外れた角度でプロファイルが平坦化しスコアが大きく落ちる。
/// ノイズ画像では角度によらずほぼ一定なので 0 に近づく。
fn sharpness(scores: &[f64]) -> f64 {
    if scores.len() < 3 {
        return 0.0;
    }
    let (_, peak) = arg_max(scores);
    if peak <= 0.0 {
        return 0.0;
    }
    let mut sorted: Vec<f64> = scores.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2];
    let rel = (peak - median) / peak;
    // rel = 0.20 でほぼ飽和。実写では 0.3〜0.6、ノイズでは 0.01 未満。
    (rel / 0.20).clamp(0.0, 1.0)
}

/// 区間 `[lo, hi]` を等間隔サンプリングし、重心でピーク位置を求める。
///
/// 単純な argmax もパラボラ補間も使わない理由:
/// エッジは Gaussian でぼけているため、スコアのピークは「±0.5px 相当」の
/// 幅で**平坦**になり(実測で ±0.09°)、その平坦部の中では 1e-6 オーダーの
/// リップルが最大値の位置を決めてしまう。左右対称な山に対して厳密に中心を
/// 返す重心(ベースラインを引いた加重平均)なら、平坦部でもリップルでも
/// ぶれない。固定サンプル数なので完全に決定論的。
fn refine_max<F: Fn(f64) -> f64>(lo: f64, hi: f64, f: F) -> f64 {
    if hi <= lo {
        return lo;
    }
    let n = REFINE_SAMPLES;
    let step = (hi - lo) / (n - 1) as f64;
    let values: Vec<f64> = (0..n).map(|i| f(lo + i as f64 * step)).collect();
    let base = values.iter().copied().fold(f64::INFINITY, f64::min);
    let mut sum_w = 0.0;
    let mut sum_wx = 0.0;
    for (i, v) in values.iter().enumerate() {
        let w = v - base;
        sum_w += w;
        sum_wx += w * (lo + i as f64 * step);
    }
    if sum_w > 0.0 {
        sum_wx / sum_w
    } else {
        0.5 * (lo + hi)
    }
}

/// Sobel 勾配から水平族 / 垂直族のエッジ点を抽出する。
fn extract_points(blurred: &GrayImage, max_abs_angle: f64) -> (Vec<EdgePoint>, Vec<EdgePoint>) {
    let (w, h) = blurred.dimensions();
    if w < 16 || h < 16 {
        return (Vec::new(), Vec::new());
    }
    let gx = horizontal_sobel(blurred);
    let gy = vertical_sobel(blurred);

    // 軸から離れすぎた勾配(斜め構造)は捨てる。
    let slack = (max_abs_angle + 6.0).to_radians().tan() as f32;
    let cx = (w - 1) as f32 / 2.0;
    let cy = (h - 1) as f32 / 2.0;

    // 強度ヒストグラムから閾値を決める(点数が多すぎるときは引き上げる)。
    let mut hist = [0u32; 2049];
    let mut candidates = 0u32;
    for y in 0..h {
        for x in 0..w {
            let ex = gx.get_pixel(x, y)[0] as f32;
            let ey = gy.get_pixel(x, y)[0] as f32;
            if !axis_aligned(ex, ey, slack) {
                continue;
            }
            let m = (ex.abs() + ey.abs()) as usize;
            hist[m.min(2048)] += 1;
            candidates += 1;
        }
    }
    // 閾値は 3 つの下限の最大値:
    // 1. 絶対下限(量子化ノイズ)
    // 2. 「候補画素の上位 KEEP_FRACTION」— 解像度に依らず同じ構造が残るよう割合で決め、
    //    総数を MAX_POINTS で頭打ちにして実行時間を保証する
    // 3. 「最も強いエッジ」に対する相対下限 — これがないと、粒状ノイズの弱い勾配が
    //    数で質量を支配し、線の少ない族の推定が乱数化する
    let keep = ((candidates as f64 * KEEP_FRACTION) as usize).clamp(1, MAX_POINTS);
    let strong = percentile_from_top(&hist, candidates, 0.005);
    let threshold = MIN_MAGNITUDE
        .max(count_threshold(&hist, keep))
        .max(STRONG_EDGE_RATIO * strong);

    let mut hp = Vec::new();
    let mut vp = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let ex = gx.get_pixel(x, y)[0] as f32;
            let ey = gy.get_pixel(x, y)[0] as f32;
            if !axis_aligned(ex, ey, slack) {
                continue;
            }
            let m = ex.abs() + ey.abs();
            if (m as f64) < threshold {
                continue;
            }
            let p = EdgePoint {
                x: x as f32 - cx,
                y: y as f32 - cy,
                w: m,
            };
            if ey.abs() > ex.abs() {
                hp.push(p);
            } else {
                vp.push(p);
            }
        }
    }
    (hp, vp)
}

/// 上位 `keep` 個だけを残すための強度閾値。
fn count_threshold(hist: &[u32], keep: usize) -> f64 {
    let mut acc = 0usize;
    for m in (0..hist.len()).rev() {
        acc += hist[m] as usize;
        if acc >= keep {
            return m as f64;
        }
    }
    0.0
}

/// 上位 `frac` の位置にある強度(例 frac=0.005 なら 99.5 パーセンタイル)。
fn percentile_from_top(hist: &[u32], total: u32, frac: f64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let target = ((total as f64 * frac).ceil() as usize).max(1);
    count_threshold(hist, target)
}

/// 勾配が座標軸から `slack`(= tan)以内かどうか。
fn axis_aligned(ex: f32, ey: f32, slack: f32) -> bool {
    if ex == 0.0 && ey == 0.0 {
        return false;
    }
    if ey.abs() > ex.abs() {
        (ex / ey).abs() <= slack
    } else {
        (ey / ex).abs() <= slack
    }
}
