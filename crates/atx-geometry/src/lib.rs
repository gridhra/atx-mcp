//! atx-geometry: 傾き検出(Canny + Hough + 角度選択ヒューリスティック)。
//!
//! 品質要件:
//! - 建築・風景写真: 人工回転テストで誤差 ±0.3° 以内
//! - 検出不能な被写体(単色・人物アップ・抽象)では正しく None を返す
//! - 自動適用はしない。検出は read-only、適用判断はホスト AI に委ねる
//!
//! # パイプライン
//!
//! 1. 長辺が `working_long_edge` 以下になるようグレースケール化 + Triangle 縮小
//!    (整数演算のみ・並列化なしで完全に決定論的)
//! 2. `imageproc::edges::canny`。閾値は Sobel 勾配強度のパーセンタイル
//!    (high = 90 パーセンタイル、low = high × 0.4、範囲クランプ付き)から適応決定
//! 3. 帯域限定・0.1° 分解能の Hough 変換([`hough`])で直線を抽出。
//!    `imageproc::hough::detect_lines` は角度ビンが 1° 固定で ±0.3° 要件を
//!    満たせないため、同じ投票法を細分ビンで自前実装している
//! 4. 各直線を「ほぼ水平」「ほぼ垂直」のバンドに分け、いずれも
//!    「画面上の時計回り傾き phi」に正規化(垂直線が phi 傾いていれば
//!    ロールも phi)
//! 5. 線長重み付きの角度ヒストグラム(0.1° ビン + σ=0.25° 平滑化)の支配ピーク
//! 6. **投影プロファイル法**([`projection`])で 5 の候補を 0.1° 未満まで細分する。
//!    Hough は「長い直線」を必要とするが、実写(社殿・板塀・階段)は短く途切れた
//!    エッジばかりで、投票のビン化とあいまって 0.1° 程度の系統誤差が残る。
//!    投影プロファイルはエッジ強度をシアーした行/列ビンに撒いて分散を最大化する
//!    手法で、短いエッジの集合でも安定する。Hough が直線を 1 本も取れない場合は
//!    投影プロファイル単独で推定する(`method = "projection_profile"`)。
//! 7. 水平族・垂直族それぞれの推定を**別々に**返す
//!    (`horizontal_angle_degrees` / `vertical_angle_degrees`)。
//!    「水平は -0.5°、垂直は 0°」のような食い違いはロール(カメラの傾き)ではなく
//!    カメラ位置・パースに由来するので、補正するかどうかの判断が変わる。
//! 8. 探索範囲全体のスコア曲線(`score_curve`)を返し、ピークの鋭さ・多峰性を
//!    クライアントが判断できるようにする。
//!
//! # 符号の規約
//!
//! 画像座標は x 右・y 下。ある直線の方向ベクトルが `(cos phi, sin phi)` の
//! とき、その線は画面上で水平から**時計回りに** phi 度傾いている
//! (y が下向きなので +y 方向 = 画面下)。水平にするには反時計回りに phi 度
//! 回す必要があるため、`recommended_angle_degrees = -phi`
//! (正 = 時計回りに回すと水平になる = atx-core の `Rotate` と同じ規約)。

mod angle;
mod hough;
mod projection;

use image::{imageops::FilterType, DynamicImage, GrayImage};
use imageproc::gradients::sobel_gradients;
use serde::Serialize;

use crate::angle::{confidence, inscribed_crop_loss_percent, support_for, AngleHistogram};
use crate::hough::{detect_lines_fine, Family};
use crate::projection::{ProjectionEstimate, ProjectionSearch};

/// 傾き検出の結果。MCP structuredContent 互換。
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TiltDetection {
    /// 推奨補正角(度、正 = 時計回りに回すと水平になる)。低信頼時は None。
    ///
    /// 水平族・垂直族を融合した「最終的な答え」。
    pub recommended_angle_degrees: Option<f64>,
    /// 0.0..=1.0。
    pub confidence: f64,
    /// 使用手法の識別子(例 "hough_projection_fused")。
    pub method: String,
    /// 上位角度候補(score 降順)。
    pub alternatives: Vec<AngleCandidate>,
    /// **水平族のみ**(地平線・軒・敷居など)から得た補正角。
    ///
    /// `vertical_angle_degrees` と大きく食い違う場合、ロール(カメラの傾き)ではなく
    /// 被写体側の非直交性・パースであることを示す。
    pub horizontal_angle_degrees: Option<f64>,
    /// 水平族推定の信頼度(0..=1、投影プロファイルのピークの鋭さ)。
    pub horizontal_confidence: f64,
    /// 水平族が持つエッジ質量の割合(0..=1、水平 + 垂直 = 1)。
    pub horizontal_support: f64,
    /// **垂直族のみ**(柱・框・建具など)から得た補正角。
    pub vertical_angle_degrees: Option<f64>,
    /// 垂直族推定の信頼度(0..=1)。
    pub vertical_confidence: f64,
    /// 垂直族が持つエッジ質量の割合(0..=1)。
    pub vertical_support: f64,
    /// 探索範囲全体のスコア曲線(補正角の昇順、score は 0..=1 に正規化、最大 300 点)。
    ///
    /// ピークの鋭さ・多峰性をクライアント側で判断するために返す。
    /// 最大値 1.0 の点は `recommended_angle_degrees`(棄却時も融合ピーク)に一致する。
    pub score_curve: Vec<ScorePoint>,
    /// 適用時の注意(クロップによる画素損失率など)。
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AngleCandidate {
    pub angle_degrees: f64,
    pub score: f64,
}

/// スコア曲線の 1 点。
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ScorePoint {
    /// 補正角(度、正 = 時計回りに回すと水平になる)。
    pub angle_degrees: f64,
    /// 0..=1 に正規化したスコア。
    pub score: f64,
}

/// 検出パラメータ。デフォルトで実用値。
pub struct DetectParams {
    /// この confidence 未満なら recommended_angle_degrees を None にする。
    pub min_confidence: f64,
    /// 探索する最大傾き角(度)。これを超える傾きは「意図的な構図」とみなし検出対象外。
    pub max_abs_angle: f64,
    /// 検出処理前に長辺をこのサイズまで縮小(速度・ノイズ耐性)。
    pub working_long_edge: u32,
}

impl Default for DetectParams {
    fn default() -> Self {
        Self {
            min_confidence: 0.5,
            max_abs_angle: 15.0,
            working_long_edge: 1024,
        }
    }
}

/// 手法識別子: Hough で粗く候補を出し、投影プロファイルで細分した(通常経路)。
const METHOD_FUSED: &str = "hough_projection_fused";
/// 手法識別子: Hough が直線を取れず、投影プロファイル単独で決めた。
const METHOD_PROJECTION: &str = "projection_profile";
/// 手法識別子: 投影プロファイルが使えず Hough 単独で決めた(退避経路)。
const METHOD_HOUGH: &str = "dominant_hv_lines";
/// これ未満の支持線数なら角度を推奨しない。
const MIN_SUPPORTING_LINES: usize = 2;
/// Canny 前の Gaussian σ(imageproc::edges::canny と同値)。
const CANNY_SIGMA: f32 = 1.4;
/// 投影プロファイルを信用する最小ピーク鋭さ(細分に使う条件)。
const PP_MIN_SHARPNESS: f64 = 0.25;
/// 投影プロファイル単独で角度を推奨する最小ピーク鋭さ。
const PP_STANDALONE_SHARPNESS: f64 = 0.45;
/// Hough 候補の周囲をこの半径(度)で細分する。
const PP_REFINE_RADIUS_DEG: f64 = 0.5;
/// 族ごとの推定を推奨角の周囲この半径(度)で取り直す。
const FAMILY_WINDOW_DEG: f64 = 1.0;
/// 水平族と垂直族がこれ以上食い違ったら警告する(度)。
const HV_DISAGREEMENT_DEG: f64 = 0.3;
/// スコア曲線で Hough 証拠に与える下限(0 だと直線の無い領域が潰れる)。
const CURVE_HOUGH_FLOOR: f64 = 0.25;
/// スコア曲線のグリッド点が取りうる最大値(1.0 は推奨角の点だけ)。
const CURVE_OTHER_MAX: f64 = 0.999;

/// 傾きを検出する。水平・垂直の支配線を Canny + Hough で抽出し、
/// 線長重み付きの角度ヒストグラムから支配角を推定する。
///
/// 同一入力に対して常に同一の結果を返す(決定論)。
pub fn detect_tilt(image: &DynamicImage, params: &DetectParams) -> TiltDetection {
    let max_abs_angle = params.max_abs_angle.clamp(0.5, 45.0);
    let (orig_w, orig_h) = (image.width(), image.height());

    let gray = downscale_gray(image, params.working_long_edge.max(64));
    if gray.width() < 16 || gray.height() < 16 {
        return empty(
            "image too small for tilt detection".to_string(),
            &ProjectionEstimate::default(),
        );
    }

    let blurred = imageproc::filter::gaussian_blur_f32(&gray, CANNY_SIGMA);

    // --- (A) 投影プロファイル: 短い・途切れたエッジでも安定する独立推定 ---
    let (search, pp) = ProjectionSearch::run(&blurred, max_abs_angle);

    // --- (B) Canny + Hough: 長い直線の粗い候補 ---
    let (low, high) = canny_thresholds(&blurred);
    let edges = imageproc::edges::canny(&gray, low, high);
    let lines = detect_lines_fine(&edges, &blurred, max_abs_angle);
    let hist = AngleHistogram::build(&lines, max_abs_angle);
    let total = hist.total();
    let peaks = if lines.is_empty() || total <= 0.0 {
        Vec::new()
    } else {
        hist.peaks(1.0, 3)
    };

    let pp_usable = pp.fused_phi_deg.is_some() && pp.fused_sharpness >= PP_MIN_SHARPNESS;
    let mut warnings = Vec::new();

    // --- (C) 融合 ---
    let (phi, conf, method, alternatives, enough_evidence) = if !peaks.is_empty() {
        // 角度の符号: phi = 画面上の時計回り傾き → 補正角はその逆符号。
        let alts: Vec<AngleCandidate> = peaks
            .iter()
            .map(|&phi| AngleCandidate {
                angle_degrees: round3(-phi),
                score: round3((hist.mass_near(phi) / total).clamp(0.0, 1.0)),
            })
            .collect();
        let peak_phi = peaks[0];
        let dominance = (hist.mass_near(peak_phi) / total).clamp(0.0, 1.0);
        let support = support_for(&lines, peak_phi);
        let conf = confidence(dominance, &support, MIN_SUPPORTING_LINES);
        if support.line_count < MIN_SUPPORTING_LINES {
            warnings.push(format!(
                "Only {} supporting line(s); not enough evidence to recommend a correction",
                support.line_count
            ));
        }
        // Hough の粗い候補を投影プロファイルで 0.1° 未満まで細分する。
        if pp_usable {
            let refined = search.refine_near(support.phi_deg, PP_REFINE_RADIUS_DEG, &pp);
            (
                refined,
                conf,
                METHOD_FUSED,
                alts,
                support.line_count >= MIN_SUPPORTING_LINES,
            )
        } else {
            (
                support.phi_deg,
                conf,
                METHOD_HOUGH,
                alts,
                support.line_count >= MIN_SUPPORTING_LINES,
            )
        }
    } else if pp_usable {
        // Hough が直線を取れない(短い・途切れたエッジばかりの)シーン。
        let phi = pp.fused_phi_deg.unwrap();
        let conf = projection_confidence(&pp);
        let strong = pp.fused_sharpness >= PP_STANDALONE_SHARPNESS;
        if !strong {
            warnings.push(
                "Projection-profile peak is broad; the scene may lack straight structure"
                    .to_string(),
            );
        }
        let alts = vec![AngleCandidate {
            angle_degrees: round3(-phi),
            score: round3(pp.fused_sharpness),
        }];
        (phi, conf, METHOD_PROJECTION, alts, strong)
    } else {
        return empty(
            "no dominant horizontal or vertical lines found".to_string(),
            &pp,
        );
    };

    let angle = -phi;

    // 族ごとの推定は「推奨角の近傍」で取り直す(大域ピークだと画面隅の別構造に
    // 引っ張られ、解像度で答えが飛ぶ)。
    let fam_h = search.family_near(Family::Horizontal, phi, FAMILY_WINDOW_DEG);
    let fam_v = search.family_near(Family::Vertical, phi, FAMILY_WINDOW_DEG);

    // 水平族 / 垂直族の食い違いは「ロールではなくパース・被写体側の非直交」を示す。
    if let (Some(h), Some(v)) = (&fam_h, &fam_v) {
        let diff = (h.phi_deg - v.phi_deg).abs();
        if diff > HV_DISAGREEMENT_DEG {
            warnings.push(format!(
                "Horizontal family says {:+.2}deg but vertical family says {:+.2}deg (difference {diff:.2}deg); this is likely camera position/perspective rather than roll",
                -h.phi_deg, -v.phi_deg
            ));
        }
    }
    if conf < params.min_confidence {
        warnings.push(format!(
            "Confidence {:.2} is below the threshold {:.2}; no correction is recommended",
            conf, params.min_confidence
        ));
    }

    let recommended = if conf >= params.min_confidence && enough_evidence {
        let loss = inscribed_crop_loss_percent(orig_w, orig_h, angle);
        warnings.push(format!(
            "Rotation + largest-inscribed-rect crop will remove ~{loss:.1}% of pixels"
        ));
        Some(round3(angle))
    } else {
        None
    };

    TiltDetection {
        recommended_angle_degrees: recommended,
        confidence: round3(conf),
        method: method.to_string(),
        alternatives,
        horizontal_angle_degrees: fam_h.as_ref().map(|f| round3(-f.phi_deg)),
        horizontal_confidence: round3(fam_h.as_ref().map_or(0.0, |f| f.sharpness)),
        horizontal_support: round3(pp.horizontal_support()),
        vertical_angle_degrees: fam_v.as_ref().map(|f| round3(-f.phi_deg)),
        vertical_confidence: round3(fam_v.as_ref().map_or(0.0, |f| f.sharpness)),
        vertical_support: round3(pp.vertical_support()),
        score_curve: curve_of(&pp, Some(&hist), angle),
        warnings,
    }
}

/// 投影プロファイル単独時の信頼度。
///
/// ピークの鋭さと、水平族・垂直族の一致度から合成する。
fn projection_confidence(pp: &ProjectionEstimate) -> f64 {
    let agreement = match (&pp.horizontal, &pp.vertical) {
        (Some(h), Some(v)) => (1.0 - (h.phi_deg - v.phi_deg).abs()).clamp(0.0, 1.0),
        _ => 0.65,
    };
    (0.60 * pp.fused_sharpness.clamp(0.0, 1.0) + 0.40 * agreement).clamp(0.0, 1.0)
}

/// スコア曲線(探索範囲全体)を出力形に変換する。
///
/// 投影プロファイルの正規化スコアに Hough の角度ヒストグラム(= 長い直線の証拠)を
/// 掛け合わせた「実際に答えを選んだ根拠」を返す。Hough 側は下限
/// [`CURVE_HOUGH_FLOOR`] を持たせ、直線が無い領域でも投影プロファイルの形が
/// 潰れないようにする。
///
/// 推奨角の点だけスコア 1.0 とし、グリッド上の点は最大 [`CURVE_OTHER_MAX`] に
/// 収める(「曲線の最大 = 推奨角」を保証しつつ、競合ピークは 1.0 直下の
/// 二番目の山として見えるようにするため)。
fn curve_of(
    pp: &ProjectionEstimate,
    hough: Option<&AngleHistogram>,
    peak_angle: f64,
) -> Vec<ScorePoint> {
    if pp.curve.is_empty() {
        return Vec::new();
    }
    let n = pp.curve.len();
    // Hough の証拠(グリッドが一致するときのみ使う)。
    let evidence: Vec<f64> = match hough.filter(|h| h.values.len() == n) {
        Some(h) => {
            let max = h.values.iter().copied().fold(0.0f64, f64::max);
            if max > 0.0 {
                h.values
                    .iter()
                    .map(|v| CURVE_HOUGH_FLOOR + (1.0 - CURVE_HOUGH_FLOOR) * (v / max))
                    .collect()
            } else {
                vec![1.0; n]
            }
        }
        None => vec![1.0; n],
    };
    let combined: Vec<f64> = pp
        .curve
        .iter()
        .zip(&evidence)
        .map(|((_, s), e)| s * e)
        .collect();
    let max = combined.iter().copied().fold(0.0f64, f64::max);
    let scale = if max > 0.0 {
        CURVE_OTHER_MAX / max
    } else {
        0.0
    };

    // 端点を保ちつつ間引く(出力サイズの上限)。
    let stride = n.div_ceil(projection::CURVE_MAX_POINTS - 1).max(1);
    let peak = round3(peak_angle);
    let mut out: Vec<ScorePoint> = Vec::with_capacity(projection::CURVE_MAX_POINTS);
    for i in (0..n).chain(std::iter::once(n - 1)) {
        if i % stride != 0 && i + 1 != n {
            continue;
        }
        // 出力は補正角の規約(= -phi)。
        let angle = round3(-pp.curve[i].0);
        if (angle - peak).abs() < projection::SCAN_STEP_DEG * 0.5
            || out.iter().any(|p| p.angle_degrees == angle)
        {
            continue;
        }
        out.push(ScorePoint {
            angle_degrees: angle,
            score: round3((combined[i] * scale).clamp(0.0, CURVE_OTHER_MAX)),
        });
    }
    out.push(ScorePoint {
        angle_degrees: peak,
        score: 1.0,
    });
    out.sort_by(|a, b| a.angle_degrees.partial_cmp(&b.angle_degrees).unwrap());
    out.truncate(projection::CURVE_MAX_POINTS);
    out
}

/// 角度が決まらなかったときの結果(族ごとの推定と曲線は診断用に残す)。
fn empty(warning: String, pp: &ProjectionEstimate) -> TiltDetection {
    TiltDetection {
        recommended_angle_degrees: None,
        confidence: 0.0,
        method: METHOD_HOUGH.to_string(),
        alternatives: Vec::new(),
        horizontal_angle_degrees: pp.horizontal.as_ref().map(|f| round3(-f.phi_deg)),
        horizontal_confidence: round3(pp.horizontal.as_ref().map_or(0.0, |f| f.sharpness)),
        horizontal_support: round3(pp.horizontal_support()),
        vertical_angle_degrees: pp.vertical.as_ref().map(|f| round3(-f.phi_deg)),
        vertical_confidence: round3(pp.vertical.as_ref().map_or(0.0, |f| f.sharpness)),
        vertical_support: round3(pp.vertical_support()),
        score_curve: pp
            .fused_phi_deg
            .map(|phi| curve_of(pp, None, -phi))
            .unwrap_or_default(),
        warnings: vec![warning],
    }
}

/// 長辺が `long_edge` 以下になるようグレースケール化して縮小する。
fn downscale_gray(image: &DynamicImage, long_edge: u32) -> GrayImage {
    let gray = image.to_luma8();
    let (w, h) = (gray.width(), gray.height());
    let long = w.max(h);
    if long <= long_edge || long == 0 {
        return gray;
    }
    let scale = long_edge as f64 / long as f64;
    let nw = ((w as f64 * scale).round() as u32).max(1);
    let nh = ((h as f64 * scale).round() as u32).max(1);
    image::imageops::resize(&gray, nw, nh, FilterType::Triangle)
}

/// Sobel 勾配強度のパーセンタイルから Canny 閾値を適応決定する。
///
/// ヒストグラム(整数ビン)ベースなので浮動小数の順序依存がなく決定論的。
fn canny_thresholds(blurred: &GrayImage) -> (f32, f32) {
    let g = sobel_gradients(blurred);
    let mut hist = [0u32; 1141];
    let mut n = 0u32;
    for p in g.pixels() {
        let v = (p[0] as usize).min(1140);
        hist[v] += 1;
        n += 1;
    }
    if n == 0 {
        return (40.0, 100.0);
    }
    let target = (n as f64 * 0.90) as u32;
    let mut acc = 0u32;
    let mut p90 = 0usize;
    for (v, c) in hist.iter().enumerate() {
        acc += c;
        if acc >= target {
            p90 = v;
            break;
        }
    }
    let high = (p90 as f32).clamp(40.0, 500.0);
    (high * 0.4, high)
}

/// 小数第 3 位に丸める(出力の安定化)。
fn round3(v: f64) -> f64 {
    let r = (v * 1000.0).round() / 1000.0;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}
