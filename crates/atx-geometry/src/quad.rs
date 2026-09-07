//! ドキュメント四角形の検出(輪郭ベース)。
//!
//! `detect_tilt`(傾き = 1 自由度)に対して、こちらは「画像内の支配的な四角形」
//! (用紙・画面・ホワイトボード・看板)を 8 自由度として取り、
//! atx-core の `perspective` op にそのまま貼れる quad を返す。
//! 関係は `detect_tilt` → `rotate` と同じで、**適用はしない**(read-only、
//! 判断はホスト AI に委ねる)。
//!
//! # パイプライン(DESIGN.md §9.12)
//!
//! 1. グレースケール化 + 長辺 ≤ `working_long_edge` へ Triangle 縮小
//!    ([`crate::downscale_gray`] を `detect_tilt` と共用)
//! 2. ガウス平滑(σ 1.0)→ `imageproc::edges::canny`(閾値は `detect_tilt` と
//!    同じ Sobel 勾配のパーセンタイル適応)→ 3x3 dilate 1 回で断線を接ぐ
//! 3. `imageproc::contours::find_contours` の**外側輪郭**のうち、
//!    外接矩形面積が `min_area_ratio` 以上のものを候補にする
//! 4. 各候補を `approximate_polygon_dp`(ε = 周長の 2%、closed)で近似し、
//!    **頂点数 4 かつ凸**のものを残す
//!    - 隅の精密化(**DESIGN からの上乗せ**): DP の頂点は「輪郭上に実在する点」なので、
//!      dilate による隅の面取りをそのまま拾って長辺の数 % ずれることがある。
//!      4 辺を直交回帰で当てはめ直し、隣接直線の交点を隅とする
//!      ([`refine_corners`]。改善にならない場合は DP の頂点へ戻す)
//! 5. スコア = 多角形面積(大きいものを優先)。同点は直角性で決める
//! 6. 座標を原寸へ戻し、tl / tr / br / bl(画面上の時計回り)に並べ替える
//! 7. 4 頂点が取れなければ `quad: null` + `no_quad_found`
//!
//! # 決定論
//!
//! - `find_contours` の走査順は決定的。`HashMap` の反復順に依存する処理は書かない
//! - 頂点の並べ替えは `atan2` を使わず**有理式の擬似角**(和で正規化した象限つき比)で行う
//!   (libm を経由しないので、プラットフォーム間で順序が揺れない)
//! - `sqrt` の結果は atx-core の `perspective` と同じく 1e-6 グリッドへ量子化する
//! - 出力座標は小数第 3 位へ丸め、`output_size_hint` は**丸めた後の quad**から
//!   計算する(= `perspective` が実際に受け取る値から計算する)ので、
//!   ヒントと `perspective` の出力寸法が必ず一致する

use image::DynamicImage;
use imageproc::contours::{find_contours, BorderType};
use imageproc::geometry::approximate_polygon_dp;
use imageproc::morphology::dilate;
use imageproc::point::Point;
use serde::Serialize;

use crate::{canny_thresholds, downscale_gray, round3};

/// Canny 前のガウス σ(DESIGN §9.12: 軽い平滑)。
const BLUR_SIGMA: f32 = 1.0;
/// `approximate_polygon_dp` の ε を周長の何割にするか。
const DP_EPSILON_RATIO: f64 = 0.02;
/// 辺サポートを数えるときに「エッジ画素が乗っている」と見なす探索半径(作業解像度の画素)。
const SUPPORT_RADIUS: i32 = 2;
/// 辺の直線当てはめで、隅の近傍として捨てる区間の割合(両端それぞれ)。
const FIT_TRIM: f64 = 0.2;
/// 1 辺の当てはめに最低限必要な輪郭点数。
const FIT_MIN_POINTS: usize = 8;
/// 精密化した隅が DP の頂点からこの割合(長辺比)以上ずれたら、精密化を捨てる。
const FIT_MAX_SHIFT_RATIO: f64 = 0.05;
/// confidence における辺サポートの寄与(残りが直角性による減点分)。
const SUPPORT_WEIGHT: f64 = 0.7;
/// `already_rectified` と見なす面積比の下限。
const RECTIFIED_AREA_RATIO: f64 = 0.97;
/// `already_rectified` と見なす「隅が画像隅に近い」距離(長辺に対する比)。
const RECTIFIED_CORNER_RATIO: f64 = 0.01;
/// 作業解像度でこれ未満の辺を持つ四角形は退化とみなす(画素)。
const MIN_EDGE_PX: f64 = 8.0;
/// 検出手法の識別子。
const METHOD_CONTOUR: &str = "contour";

/// quad が null になった理由(`warnings` の先頭トークン。MCP 層が英語のまま返す)。
const REASON_NO_QUAD: &str = "no_quad_found";
const REASON_ALREADY_RECTIFIED: &str = "already_rectified";
const REASON_LOW_CONFIDENCE: &str = "low_confidence";

/// 検出パラメータ。
pub struct DocumentParams {
    /// 画像面積に対する最小の四角形面積比。これ未満の候補は採らない。
    pub min_area_ratio: f64,
    /// これ未満の confidence なら `quad` を None にする(`low_confidence`)。
    pub min_confidence: f64,
    /// 検出処理前に長辺をこのサイズまで縮小する。
    pub working_long_edge: u32,
}

impl Default for DocumentParams {
    fn default() -> Self {
        Self {
            min_area_ratio: 0.2,
            min_confidence: 0.4,
            working_long_edge: 1024,
        }
    }
}

/// `perspective` の出力寸法(atx-core の「対辺長の相加平均」規則と同一)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct OutputSizeHint {
    pub width: u32,
    pub height: u32,
}

/// そのまま recipe の先頭に貼れる `perspective` op。
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct SuggestedOperation {
    /// 常に `"perspective"`。
    pub op: String,
    /// tl, tr, br, bl。
    pub quad: [[f64; 2]; 4],
}

/// ドキュメント四角形の検出結果。MCP structuredContent 互換。
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct DocumentDetection {
    /// EXIF orientation 正規化後の画素座標、tl / tr / br / bl。
    /// None は「補正しない」で、理由は `warnings` の先頭に入る。
    pub quad: Option<[[f64; 2]; 4]>,
    /// 0..=1。辺サポートを主成分に、直角性で減点したもの。
    pub confidence: f64,
    /// 検出四角形が画像面積に占める割合(0..=1)。候補が無ければ 0。
    pub area_ratio: f64,
    /// `perspective` を適用したときの出力寸法。`quad` が None なら None。
    pub output_size_hint: Option<OutputSizeHint>,
    /// 使用手法(`"contour"`)。候補が 1 つも無ければ None。
    pub method: Option<String>,
    /// `quad` が非 None のときだけ載る、貼り付け用の op。
    pub suggested_operation: Option<SuggestedOperation>,
    pub warnings: Vec<String>,
}

impl DocumentDetection {
    /// 候補すら無かったときの結果。
    fn none(reason: &str, detail: &str) -> Self {
        Self {
            quad: None,
            confidence: 0.0,
            area_ratio: 0.0,
            output_size_hint: None,
            method: None,
            suggested_operation: None,
            warnings: vec![format!("{reason}: {detail}")],
        }
    }
}

/// 画像内の支配的な四角形を検出する(read-only、決定論)。
///
/// 同一入力に対して常に同一の結果を返す。
pub fn detect_document(image: &DynamicImage, params: &DocumentParams) -> DocumentDetection {
    let min_area_ratio = params.min_area_ratio.clamp(0.0, 1.0);
    let (orig_w, orig_h) = (image.width(), image.height());
    if orig_w == 0 || orig_h == 0 {
        return DocumentDetection::none(REASON_NO_QUAD, "the image is empty");
    }

    let gray = downscale_gray(image, params.working_long_edge.max(64));
    let (work_w, work_h) = (gray.width(), gray.height());
    if work_w < 16 || work_h < 16 {
        return DocumentDetection::none(
            REASON_NO_QUAD,
            "the image is too small for contour-based quad detection",
        );
    }

    // --- エッジ地図(detect_tilt と同じ閾値適応。dilate で輪郭の断線を接ぐ) ---
    let blurred = imageproc::filter::gaussian_blur_f32(&gray, BLUR_SIGMA);
    let (low, high) = canny_thresholds(&blurred);
    let edges = imageproc::edges::canny(&gray, low, high);
    let closed = dilate(&edges, imageproc::distance_transform::Norm::LInf, 1);

    // --- 候補の抽出 ---
    let area_px = work_w as f64 * work_h as f64;
    let min_area = min_area_ratio * area_px;
    let mut best: Option<Candidate> = None;
    for contour in find_contours::<i32>(&closed) {
        if contour.border_type != BorderType::Outer || contour.points.len() < 4 {
            continue;
        }
        // まず外接矩形で足切りする(多角形近似より遥かに安い)。
        if bbox_area(&contour.points) < min_area {
            continue;
        }
        let epsilon = quantize_1e6(perimeter(&contour.points) * DP_EPSILON_RATIO);
        let approx = approximate_polygon_dp(&contour.points, epsilon, true);
        let Some(quad) = quad_from_approximation(&approx) else {
            continue;
        };
        // DP の頂点は「輪郭上に実在する点」なので、隅のドット 1 個の癖や
        // dilate による面取りをそのまま拾う。4 辺を直線当てはめして交点を取り直す。
        let quad = refine_corners(&contour.points, quad, work_w.max(work_h) as f64);
        let area = polygon_area(&quad);
        if area < min_area {
            continue;
        }
        let candidate = Candidate {
            quad,
            area,
            rectangularity: rectangularity(&quad),
        };
        // 面積優先、同点は直角性で決める(どちらも決定論的な f64 比較)。
        let better = match &best {
            None => true,
            Some(b) => {
                candidate.area > b.area
                    || (candidate.area == b.area && candidate.rectangularity > b.rectangularity)
            }
        };
        if better {
            best = Some(candidate);
        }
    }

    let Some(candidate) = best else {
        return DocumentDetection::none(
            REASON_NO_QUAD,
            &format!(
                "no convex quadrilateral covering at least {:.0}% of the frame was found; \
                 pass an explicit perspective quad if you can see one",
                min_area_ratio * 100.0
            ),
        );
    };

    // --- 作業解像度 → 原寸 ---
    let sx = orig_w as f64 / work_w as f64;
    let sy = orig_h as f64 / work_h as f64;
    let mut quad = [[0f64; 2]; 4];
    for (i, p) in candidate.quad.iter().enumerate() {
        quad[i] = [round3(p[0] * sx), round3(p[1] * sy)];
    }

    let area_ratio = round3((candidate.area / area_px).clamp(0.0, 1.0));
    let support = edge_support(&closed, &candidate.quad);
    let confidence = round3(
        (support * (SUPPORT_WEIGHT + (1.0 - SUPPORT_WEIGHT) * candidate.rectangularity))
            .clamp(0.0, 1.0),
    );
    let method = Some(METHOD_CONTOUR.to_string());

    // 画像枠とほぼ一致する四角形は「もう補正済み」。perspective を掛けても
    // 画素を混ぜ直すだけ損なので、quad を返さない。
    if is_already_rectified(&quad, orig_w, orig_h, area_ratio) {
        return DocumentDetection {
            quad: None,
            confidence,
            area_ratio,
            output_size_hint: None,
            method,
            suggested_operation: None,
            warnings: vec![format!(
                "{REASON_ALREADY_RECTIFIED}: the detected quad already fills the frame \
                 ({:.0}% of the area, corners within 1% of the image corners); \
                 no perspective correction is needed",
                area_ratio * 100.0
            )],
        };
    }

    if confidence < params.min_confidence {
        return DocumentDetection {
            quad: None,
            confidence,
            area_ratio,
            output_size_hint: None,
            method,
            suggested_operation: None,
            warnings: vec![format!(
                "{REASON_LOW_CONFIDENCE}: the best quad scored {confidence:.2}, below the \
                 threshold {:.2}; correcting it would probably make things worse",
                params.min_confidence
            )],
        };
    }

    DocumentDetection {
        quad: Some(quad),
        confidence,
        area_ratio,
        // ヒントは**丸めた後の quad** から計算する(perspective が受け取る値そのもの)。
        output_size_hint: Some(output_size_hint(&quad)),
        method,
        suggested_operation: Some(SuggestedOperation {
            op: "perspective".to_string(),
            quad,
        }),
        warnings: Vec::new(),
    }
}

/// 候補の四角形(作業解像度の座標)。
struct Candidate {
    quad: [[f64; 2]; 4],
    area: f64,
    rectangularity: f64,
}

/// `perspective` の出力寸法規則(対辺長の相加平均を丸める)。
///
/// atx-core `ops::perspective::quad_homography` と**同じ式・同じ量子化**であること。
/// ここがずれると `output_size_hint` と実際の出力寸法が食い違う。
fn output_size_hint(q: &[[f64; 2]; 4]) -> OutputSizeHint {
    let top = edge_len(q[0], q[1]);
    let bottom = edge_len(q[3], q[2]);
    let left = edge_len(q[0], q[3]);
    let right = edge_len(q[1], q[2]);
    let dim = |v: f64| -> u32 { v.round().clamp(1.0, u32::MAX as f64) as u32 };
    OutputSizeHint {
        width: dim((top + bottom) / 2.0),
        height: dim((left + right) / 2.0),
    }
}

/// 2 点間距離(sqrt は即 1e-6 量子化 = atx-core の規約)。
fn edge_len(a: [f64; 2], b: [f64; 2]) -> f64 {
    quantize_1e6(((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt())
}

/// atx-core `transform::quantize_1e6` と同じ量子化。
fn quantize_1e6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

/// 輪郭点列の外接矩形面積。
fn bbox_area(points: &[Point<i32>]) -> f64 {
    let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for p in points {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    ((x1 - x0) as f64 + 1.0) * ((y1 - y0) as f64 + 1.0)
}

/// 閉じた輪郭の周長。
fn perimeter(points: &[Point<i32>]) -> f64 {
    let mut total = 0.0;
    for i in 0..points.len() {
        let a = points[i];
        let b = points[(i + 1) % points.len()];
        total += edge_len([a.x as f64, a.y as f64], [b.x as f64, b.y as f64]);
    }
    total
}

/// 多角形近似から「tl, tr, br, bl の順に並んだ厳密に凸な四角形」を取り出す。
///
/// - 頂点数は 4 のみ(始点の重複は落とす)
/// - 重心まわりの擬似角で画面上の時計回りに並べ、最も左上に近い頂点を tl に回す
/// - atx-core `perspective` の `is_strictly_convex_in_order` と同じ条件を課すので、
///   返した quad はそのまま recipe に貼って validate を通る
fn quad_from_approximation(approx: &[Point<i32>]) -> Option<[[f64; 2]; 4]> {
    let mut pts: Vec<[f64; 2]> = Vec::with_capacity(approx.len());
    for p in approx {
        let p = [p.x as f64, p.y as f64];
        if pts.last().is_some_and(|last| *last == p) {
            continue;
        }
        pts.push(p);
    }
    if pts.len() > 1 && pts.first() == pts.last() {
        pts.pop();
    }
    if pts.len() != 4 {
        return None;
    }

    // 重心まわりに画面上の時計回りへ並べる。
    let cx = (pts[0][0] + pts[1][0] + pts[2][0] + pts[3][0]) / 4.0;
    let cy = (pts[0][1] + pts[1][1] + pts[2][1] + pts[3][1]) / 4.0;
    pts.sort_by(|a, b| {
        let ka = pseudo_angle(a[0] - cx, a[1] - cy);
        let kb = pseudo_angle(b[0] - cx, b[1] - cy);
        // 擬似角が同値になるのは中心から見て同じ向きの 2 点(退化)。
        // その場合は座標で決めて順序を安定させる。
        ka.partial_cmp(&kb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal))
            .then(a[1].partial_cmp(&b[1]).unwrap_or(std::cmp::Ordering::Equal))
    });

    // 最も左上に近い頂点(x + y 最小、同点は x → y)を先頭へ回す。
    let mut tl = 0usize;
    for i in 1..4 {
        let (s, best) = (pts[i][0] + pts[i][1], pts[tl][0] + pts[tl][1]);
        if s < best || (s == best && (pts[i][0], pts[i][1]) < (pts[tl][0], pts[tl][1])) {
            tl = i;
        }
    }
    let quad = [
        pts[tl],
        pts[(tl + 1) % 4],
        pts[(tl + 2) % 4],
        pts[(tl + 3) % 4],
    ];

    if !is_strictly_convex_in_order(&quad) {
        return None;
    }
    for i in 0..4 {
        if edge_len(quad[i], quad[(i + 1) % 4]) < MIN_EDGE_PX {
            return None;
        }
    }
    Some(quad)
}

/// DP で得た 4 隅を、4 辺の**直線当てはめの交点**として取り直す。
///
/// Douglas-Peucker が返すのは「輪郭上に実在する点」なので、隅は
/// エッジ帯の面取り(dilate + Canny の帯幅)や 1 画素の凹凸をそのまま拾い、
/// 長辺の数 % ずれることがある。辺は数百点の直線証拠を持っているので、
/// 隅の近傍([`FIT_TRIM`] 分)を捨てて全長最小二乗(直交回帰)で 4 本の直線を求め、
/// 隣り合う直線の交点を隅とする方がはるかに安定する。
///
/// 当てはめが退化した / 交点が DP 頂点から [`FIT_MAX_SHIFT_RATIO`] 以上離れた /
/// 凸性が壊れた場合は**元の quad をそのまま返す**(精密化は改善のみを許す)。
///
/// 決定論: 総和は輪郭点の走査順(= `find_contours` の決定的な順)に左結合で積む。
fn refine_corners(points: &[Point<i32>], quad: [[f64; 2]; 4], long_edge: f64) -> [[f64; 2]; 4] {
    // 各点を最も近い辺へ割り当て、隅の近傍を捨てて総和を貯める。
    let mut acc = [LineAccumulator::default(); 4];
    for p in points {
        let p = [p.x as f64, p.y as f64];
        let mut best = (f64::INFINITY, 0usize, 0.0f64);
        for side in 0..4 {
            let (d2, t) = point_to_segment(p, quad[side], quad[(side + 1) % 4]);
            if d2 < best.0 {
                best = (d2, side, t);
            }
        }
        let (_, side, t) = best;
        if !(FIT_TRIM..=(1.0 - FIT_TRIM)).contains(&t) {
            continue;
        }
        acc[side].push(p);
    }

    let mut lines = [([0.0f64; 2], [0.0f64; 2]); 4];
    for side in 0..4 {
        match acc[side].fit() {
            Some(line) => lines[side] = line,
            None => return quad,
        }
    }

    let limit = long_edge * FIT_MAX_SHIFT_RATIO;
    let mut refined = quad;
    for corner in 0..4 {
        // 隅 i は 辺 (i-1) と 辺 i の交点。
        let a = lines[(corner + 3) % 4];
        let b = lines[corner];
        let Some(p) = intersect(a, b) else {
            return quad;
        };
        if edge_len(p, quad[corner]) > limit {
            return quad;
        }
        refined[corner] = [quantize_1e6(p[0]), quantize_1e6(p[1])];
    }
    if !is_strictly_convex_in_order(&refined) {
        return quad;
    }
    refined
}

/// 1 辺分の直交回帰(全長最小二乗)の総和。
#[derive(Debug, Clone, Copy, Default)]
struct LineAccumulator {
    n: f64,
    sx: f64,
    sy: f64,
    sxx: f64,
    sxy: f64,
    syy: f64,
}

impl LineAccumulator {
    fn push(&mut self, p: [f64; 2]) {
        self.n += 1.0;
        self.sx += p[0];
        self.sy += p[1];
        self.sxx += p[0] * p[0];
        self.sxy += p[0] * p[1];
        self.syy += p[1] * p[1];
    }

    /// (通過点, 方向ベクトル)。点が少なすぎる / 分散が 0 の場合は None。
    ///
    /// 2x2 共分散行列の最大固有値に対応する固有ベクトルが最適方向。
    /// 対称 2x2 なので閉形式で書ける(`sqrt` は 1e-6 量子化)。
    fn fit(&self) -> Option<([f64; 2], [f64; 2])> {
        if self.n < FIT_MIN_POINTS as f64 {
            return None;
        }
        let mx = self.sx / self.n;
        let my = self.sy / self.n;
        let cxx = self.sxx / self.n - mx * mx;
        let cxy = self.sxy / self.n - mx * my;
        let cyy = self.syy / self.n - my * my;
        let trace = cxx + cyy;
        let diff = cxx - cyy;
        let root = quantize_1e6((diff * diff + 4.0 * cxy * cxy).sqrt());
        let lambda = (trace + root) / 2.0;
        let dir = if cxy.abs() > 1e-9 {
            [cxy, lambda - cxx]
        } else if cxx >= cyy {
            [1.0, 0.0]
        } else {
            [0.0, 1.0]
        };
        let norm = edge_len([0.0, 0.0], dir);
        if norm <= 0.0 {
            return None;
        }
        Some(([mx, my], [dir[0] / norm, dir[1] / norm]))
    }
}

/// 2 直線((通過点, 方向)形式)の交点。平行に近ければ None。
fn intersect(a: ([f64; 2], [f64; 2]), b: ([f64; 2], [f64; 2])) -> Option<[f64; 2]> {
    let (p, d) = a;
    let (q, e) = b;
    let denom = d[0] * e[1] - d[1] * e[0];
    if denom.abs() < 1e-6 {
        return None;
    }
    let t = ((q[0] - p[0]) * e[1] - (q[1] - p[1]) * e[0]) / denom;
    Some([p[0] + d[0] * t, p[1] + d[1] * t])
}

/// 点と線分の (距離の 2 乗, 射影パラメータ t)。
fn point_to_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> (f64, f64) {
    let vx = b[0] - a[0];
    let vy = b[1] - a[1];
    let len2 = vx * vx + vy * vy;
    let t = if len2 <= 0.0 {
        0.0
    } else {
        (((p[0] - a[0]) * vx + (p[1] - a[1]) * vy) / len2).clamp(0.0, 1.0)
    };
    let dx = p[0] - (a[0] + vx * t);
    let dy = p[1] - (a[1] + vy * t);
    (dx * dx + dy * dy, t)
}

/// `atan2` を使わない単調な擬似角(0..4)。
///
/// x 右・y 下の画像座標で、+x → +y → -x → -y(= 画面上の時計回り)の順に
/// 単調増加する。四則演算と絶対値だけなので libm を経由せず、
/// プラットフォーム間で並べ替え結果が揺れない。
fn pseudo_angle(dx: f64, dy: f64) -> f64 {
    let sum = dx.abs() + dy.abs();
    if sum == 0.0 {
        return 0.0;
    }
    let a = dx / sum;
    if dy >= 0.0 {
        1.0 - a
    } else {
        3.0 + a
    }
}

/// tl, tr, br, bl の順に並んだ**厳密に凸**な四角形か
/// (atx-core `ops::perspective` と同一の判定)。
fn is_strictly_convex_in_order(q: &[[f64; 2]; 4]) -> bool {
    (0..4).all(|i| {
        let a = q[i];
        let b = q[(i + 1) % 4];
        let c = q[(i + 2) % 4];
        let cross = (b[0] - a[0]) * (c[1] - b[1]) - (b[1] - a[1]) * (c[0] - b[0]);
        cross > 0.0
    })
}

/// 多角形面積(靴紐公式)。
fn polygon_area(q: &[[f64; 2]; 4]) -> f64 {
    let mut sum = 0.0;
    for i in 0..4 {
        let a = q[i];
        let b = q[(i + 1) % 4];
        sum += a[0] * b[1] - b[0] * a[1];
    }
    (sum / 2.0).abs()
}

/// 直角性 0..=1(4 内角の 90° からの平均偏差が 0 なら 1、30° 以上で 0)。
///
/// `acos` を避けるため、角度そのものではなく**正規化した内積**
/// `|cos θ| = |u·v| / (|u||v|)` の平均から求める。直角なら 0、鋭角/鈍角ほど 1 に近づく。
/// 30° の偏差に相当する `cos(60°)`... ではなく `cos(90° - 30°) = 0.866` を境界にした
/// 線形写像で 0..1 に落とす(それ以上ゆがんだ四角形は「直角性 0」)。
fn rectangularity(q: &[[f64; 2]; 4]) -> f64 {
    // cos(90° - RECT_DEV_FULL) = cos(30°)。定数式なので実行時に libm を呼ばない。
    const COS_LIMIT: f64 = 0.866_025_403_784_438_6;
    let mut total = 0.0;
    for i in 0..4 {
        let prev = q[(i + 3) % 4];
        let cur = q[i];
        let next = q[(i + 1) % 4];
        let u = [prev[0] - cur[0], prev[1] - cur[1]];
        let v = [next[0] - cur[0], next[1] - cur[1]];
        let nu = edge_len([0.0, 0.0], u);
        let nv = edge_len([0.0, 0.0], v);
        if nu == 0.0 || nv == 0.0 {
            return 0.0;
        }
        let cos = ((u[0] * v[0] + u[1] * v[1]) / (nu * nv)).abs();
        total += (1.0 - cos / COS_LIMIT).clamp(0.0, 1.0);
    }
    total / 4.0
}

/// 辺サポート: quad の周上を 1 画素刻みで歩き、半径 [`SUPPORT_RADIUS`] 以内に
/// エッジ画素があるサンプルの割合(0..=1)。
///
/// 「輪郭近似が実在のエッジに乗っているか」を測る量で、平坦な領域を
/// 無理やり四角形に近似したケース(= 補正すると壊れるケース)を落とす。
fn edge_support(edges: &image::GrayImage, q: &[[f64; 2]; 4]) -> f64 {
    let (w, h) = (edges.width() as i32, edges.height() as i32);
    let mut total = 0u32;
    let mut hit = 0u32;
    for i in 0..4 {
        let a = q[i];
        let b = q[(i + 1) % 4];
        let steps = (edge_len(a, b).round() as u32).max(1);
        for s in 0..=steps {
            let t = s as f64 / steps as f64;
            let x = (a[0] + (b[0] - a[0]) * t).round() as i32;
            let y = (a[1] + (b[1] - a[1]) * t).round() as i32;
            total += 1;
            let mut found = false;
            for dy in -SUPPORT_RADIUS..=SUPPORT_RADIUS {
                for dx in -SUPPORT_RADIUS..=SUPPORT_RADIUS {
                    let (px, py) = (x + dx, y + dy);
                    if px >= 0
                        && py >= 0
                        && px < w
                        && py < h
                        && edges.get_pixel(px as u32, py as u32)[0] > 0
                    {
                        found = true;
                    }
                }
            }
            if found {
                hit += 1;
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        hit as f64 / total as f64
    }
}

/// 検出四角形が画像枠とほぼ一致しているか(= 補正の必要が無い)。
fn is_already_rectified(q: &[[f64; 2]; 4], w: u32, h: u32, area_ratio: f64) -> bool {
    if area_ratio <= RECTIFIED_AREA_RATIO {
        return false;
    }
    let tolerance = w.max(h) as f64 * RECTIFIED_CORNER_RATIO;
    let corners = [
        [0.0, 0.0],
        [w as f64, 0.0],
        [w as f64, h as f64],
        [0.0, h as f64],
    ];
    (0..4).all(|i| edge_len(q[i], corners[i]) <= tolerance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use imageproc::drawing::draw_polygon_mut;

    /// 暗い背景に、指定 quad の明るい四角形を描いた合成画像。
    fn synthetic(w: u32, h: u32, quad: &[[f64; 2]; 4]) -> DynamicImage {
        let mut img = RgbImage::from_pixel(w, h, Rgb([24, 22, 20]));
        let poly: Vec<Point<i32>> = quad
            .iter()
            .map(|p| Point::new(p[0].round() as i32, p[1].round() as i32))
            .collect();
        draw_polygon_mut(&mut img, &poly, Rgb([238, 236, 232]));
        DynamicImage::ImageRgb8(img)
    }

    fn max_corner_error(a: &[[f64; 2]; 4], b: &[[f64; 2]; 4]) -> f64 {
        (0..4).fold(0.0f64, |acc, i| acc.max(edge_len(a[i], b[i])))
    }

    #[test]
    fn detects_a_trapezoid_within_one_percent_of_the_long_edge() {
        let expected = [
            [120.0, 90.0],
            [700.0, 150.0],
            [660.0, 560.0],
            [160.0, 520.0],
        ];
        let image = synthetic(800, 600, &expected);
        let detection = detect_document(&image, &DocumentParams::default());

        let quad = detection
            .quad
            .expect("a bright quad on a dark ground must be detected");
        let error = max_corner_error(&quad, &expected);
        assert!(
            error <= 8.0,
            "corner error {error:.2}px exceeds 1% of the 800px long edge ({detection:?})"
        );
        assert!(detection.confidence > 0.5, "{detection:?}");
        assert_eq!(detection.method.as_deref(), Some(METHOD_CONTOUR));
        assert_eq!(
            detection
                .suggested_operation
                .as_ref()
                .map(|s| s.op.as_str()),
            Some("perspective")
        );
        assert_eq!(
            detection.suggested_operation.as_ref().map(|s| s.quad),
            Some(quad)
        );
        assert!(detection.output_size_hint.is_some());
    }

    #[test]
    fn a_page_filling_the_frame_is_reported_as_already_rectified() {
        let w = 640u32;
        let h = 480u32;
        // 枠いっぱいの用紙(境界から 2px だけ内側 = 面積比 > 0.98、
        // 隅も画像の隅から長辺 1% 以内)。
        let inset = 2.0;
        let quad = [
            [inset, inset],
            [w as f64 - inset, inset],
            [w as f64 - inset, h as f64 - inset],
            [inset, h as f64 - inset],
        ];
        let detection = detect_document(&synthetic(w, h, &quad), &DocumentParams::default());
        assert!(detection.quad.is_none(), "{detection:?}");
        assert!(
            detection.warnings[0].starts_with(REASON_ALREADY_RECTIFIED),
            "{detection:?}"
        );
        assert!(detection.area_ratio > RECTIFIED_AREA_RATIO, "{detection:?}");
    }

    #[test]
    fn a_uniform_image_yields_no_quad() {
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(400, 300, Rgb([180, 180, 180])));
        let detection = detect_document(&image, &DocumentParams::default());
        assert!(detection.quad.is_none(), "{detection:?}");
        assert_eq!(detection.method, None);
        assert!(
            detection.warnings[0].starts_with(REASON_NO_QUAD),
            "{detection:?}"
        );
        assert_eq!(detection.confidence, 0.0);
        assert_eq!(detection.area_ratio, 0.0);
    }

    #[test]
    fn detection_is_deterministic() {
        let quad = [
            [100.0, 80.0],
            [640.0, 130.0],
            [600.0, 520.0],
            [140.0, 470.0],
        ];
        let image = synthetic(720, 600, &quad);
        let first = detect_document(&image, &DocumentParams::default());
        let second = detect_document(&image, &DocumentParams::default());
        assert_eq!(first, second);
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
    }

    #[test]
    fn the_size_hint_matches_the_perspective_output_rule() {
        // 対辺長の相加平均 → 丸め。単純な台形で手計算と突き合わせる。
        let quad = [[0.0, 0.0], [100.0, 0.0], [80.0, 60.0], [20.0, 60.0]];
        let hint = output_size_hint(&quad);
        assert_eq!(hint.width, 80); // (100 + 60) / 2
        let side = edge_len([0.0, 0.0], [20.0, 60.0]);
        assert_eq!(hint.height, side.round() as u32);
    }

    #[test]
    fn min_area_ratio_rejects_a_small_quad() {
        // 画像の ~6% しか占めない四角形。既定(0.2)では棄却、0.05 なら拾う。
        let quad = [[40.0, 40.0], [240.0, 40.0], [240.0, 190.0], [40.0, 190.0]];
        let image = synthetic(800, 600, &quad);
        assert!(detect_document(&image, &DocumentParams::default())
            .quad
            .is_none());
        let loose = DocumentParams {
            min_area_ratio: 0.05,
            ..Default::default()
        };
        assert!(detect_document(&image, &loose).quad.is_some());
    }

    proptest::proptest! {
        // 1 ケースあたり Canny + 輪郭抽出を丸ごと回すので、既定の 256 ケースは重すぎる。
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(16))]

        /// 任意の(十分に大きい)凸四角形を描いた合成画像で quad が返るなら、
        /// その順序は必ず tl, tr, br, bl の厳密凸でなければならない
        /// (= atx-core `perspective` の validate をそのまま通る)。
        #[test]
        fn returned_quads_are_convex_in_tl_tr_br_bl_order(
            dx0 in 20.0f64..80.0,
            dy0 in 20.0f64..80.0,
            dx1 in 20.0f64..80.0,
            dy1 in 20.0f64..80.0,
            dx2 in 20.0f64..80.0,
            dy2 in 20.0f64..80.0,
            dx3 in 20.0f64..80.0,
            dy3 in 20.0f64..80.0,
        ) {
            let (w, h) = (320u32, 260u32);
            // 4 隅の内側へ寄せた点を取ると、必ず凸四角形になる。
            let quad = [
                [dx0, dy0],
                [w as f64 - dx1, dy1],
                [w as f64 - dx2, h as f64 - dy2],
                [dx3, h as f64 - dy3],
            ];
            let image = synthetic(w, h, &quad);
            let detection = detect_document(&image, &DocumentParams::default());
            if let Some(found) = detection.quad {
                proptest::prop_assert!(
                    is_strictly_convex_in_order(&found),
                    "detected quad {found:?} is not strictly convex in tl,tr,br,bl order"
                );
                // tl は必ず「最も左上」に来ている。
                let tl_sum = found[0][0] + found[0][1];
                for p in &found[1..] {
                    proptest::prop_assert!(tl_sum <= p[0] + p[1]);
                }
            }
        }
    }
}
