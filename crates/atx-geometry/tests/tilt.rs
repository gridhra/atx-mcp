//! detect_tilt の精度・棄却・決定論テスト。
//!
//! 合成画像は「回転処理」ではなく**傾いた構造を直接アンチエイリアス描画**して
//! 生成する(回転補間による偽エッジ・境界パディングの影響を避けるため)。

use atx_geometry::{detect_tilt, DetectParams, TiltDetection};
use image::{DynamicImage, GrayImage, Luma};

/// 画面上の時計回り傾き phi[deg] を与え、
/// 「水平方向の縞 + 垂直方向の縞」からなる格子画像を生成する。
///
/// - u = x cos phi + y sin phi (傾いた「水平」方向に沿う座標)
/// - v = -x sin phi + y cos phi (それに直交する座標)
///
/// v が spacing の倍数付近の画素を暗くすると、方向ベクトル (cos phi, sin phi)
/// の線 = 画面上で時計回りに phi 度傾いた「水平線」になる。
fn grid_image(w: u32, h: u32, phi_deg: f64, spacing: f64, verticals: bool) -> DynamicImage {
    let (s, c) = phi_deg.to_radians().sin_cos();
    let cx = (w - 1) as f64 / 2.0;
    let cy = (h - 1) as f64 / 2.0;
    let mut img = GrayImage::from_pixel(w, h, Luma([230]));
    for y in 0..h {
        for x in 0..w {
            let dx = x as f64 - cx;
            let dy = y as f64 - cy;
            let u = dx * c + dy * s;
            let v = -dx * s + dy * c;
            let mut k = stripe(v, spacing);
            if verticals {
                k = k.max(stripe(u, spacing));
            }
            let val = 230.0 - 200.0 * k;
            img.put_pixel(x, y, Luma([val.round().clamp(0.0, 255.0) as u8]));
        }
    }
    DynamicImage::ImageLuma8(img)
}

/// spacing 間隔の縞プロファイル(半値幅 1.5px + 1px のアンチエイリアス勾配)。
fn stripe(t: f64, spacing: f64) -> f64 {
    let d = (t / spacing).round().mul_add(-spacing, t).abs();
    (1.5 - d).clamp(0.0, 1.0)
}

/// 3 バンド(地平線が 2 本)だけの画像。垂直線は含まない。
fn horizon_image(w: u32, h: u32, phi_deg: f64) -> DynamicImage {
    let (s, c) = phi_deg.to_radians().sin_cos();
    let cx = (w - 1) as f64 / 2.0;
    let cy = (h - 1) as f64 / 2.0;
    let mut img = GrayImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let dx = x as f64 - cx;
            let dy = y as f64 - cy;
            let v = -dx * s + dy * c;
            // v < -60: 空(明) / -60..60: 山(中) / 60 <: 地面(暗)
            let val = band(v, -60.0, 210.0, 120.0) + band(v, 60.0, 0.0, -80.0);
            img.put_pixel(x, y, Luma([val.round().clamp(0.0, 255.0) as u8]));
        }
    }
    DynamicImage::ImageLuma8(img)
}

/// 境界 edge を挟んで lo → hi へ 1px で滑らかに遷移する値。
fn band(v: f64, edge: f64, lo: f64, hi: f64) -> f64 {
    let t = ((v - edge) / 1.0 + 0.5).clamp(0.0, 1.0);
    lo + (hi - lo) * t
}

/// 実写に近い「破線・短いセグメント + 粒状ノイズ」の合成画像。
///
/// Hough が苦手(1 本 1 本が短く途切れている)で、投影プロファイルが得意な形。
/// - `phi_h_deg`: 水平族の画面上の時計回り傾き
/// - `phi_v_deg`: 垂直族の傾き(None なら垂直線を描かない)
fn dashed_image(w: u32, h: u32, phi_h_deg: f64, phi_v_deg: Option<f64>) -> DynamicImage {
    let mut img = GrayImage::from_pixel(w, h, Luma([200]));
    let mut state: u32 = 0xC0FF_EE01;
    let mut rnd = move || {
        // xorshift32(決定論的)
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    let cx = (w - 1) as f64 / 2.0;
    let cy = (h - 1) as f64 / 2.0;

    // 水平族: y = y0 + x tan(phi)
    let th = phi_h_deg.to_radians().tan();
    for k in -3..=3 {
        let y0 = cy + k as f64 * 55.0;
        let mut x = 8.0;
        while x < w as f64 - 8.0 {
            let end = (x + 18.0 + (rnd() % 40) as f64).min(w as f64 - 8.0);
            let gap = 6.0 + (rnd() % 25) as f64;
            let mut t = x;
            while t < end {
                splat(&mut img, t, y0 + (t - cx) * th, 150.0);
                t += 0.5;
            }
            x = end + gap;
        }
    }

    // 垂直族: x = x0 - y tan(phi)
    if let Some(phi_v) = phi_v_deg {
        let tv = phi_v.to_radians().tan();
        for k in -3..=3 {
            let x0 = cx + k as f64 * 60.0;
            let mut y = 8.0;
            while y < h as f64 - 8.0 {
                let end = (y + 15.0 + (rnd() % 35) as f64).min(h as f64 - 8.0);
                let gap = 6.0 + (rnd() % 20) as f64;
                let mut t = y;
                while t < end {
                    splat(&mut img, x0 - (t - cy) * tv, t, 150.0);
                    t += 0.5;
                }
                y = end + gap;
            }
        }
    }

    for p in img.pixels_mut() {
        let n = (rnd() % 21) as i32 - 10;
        p[0] = (p[0] as i32 + n).clamp(0, 255) as u8;
    }
    DynamicImage::ImageLuma8(img)
}

/// サブピクセル位置にバイリニアで暗さを置く。
fn splat(img: &mut GrayImage, x: f64, y: f64, dark: f64) {
    let (w, h) = img.dimensions();
    for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
        let px = x.floor() + dx;
        let py = y.floor() + dy;
        if px < 0.0 || py < 0.0 || px >= w as f64 || py >= h as f64 {
            continue;
        }
        let wgt = (1.0 - (px - x).abs()).max(0.0) * (1.0 - (py - y).abs()).max(0.0);
        let p = img.get_pixel_mut(px as u32, py as u32);
        p[0] = (p[0] as f64 - dark * wgt).clamp(0.0, 255.0) as u8;
    }
}

fn params(long_edge: u32) -> DetectParams {
    DetectParams {
        working_long_edge: long_edge,
        ..Default::default()
    }
}

fn describe(name: &str, d: &TiltDetection) {
    println!(
        "[{name}] angle={:?} confidence={:.3} method={} alternatives={:?} warnings={:?}",
        d.recommended_angle_degrees, d.confidence, d.method, d.alternatives, d.warnings
    );
}

/// 想定角度セット(recommended_angle_degrees の期待値)。
const EXPECTED: [f64; 7] = [-8.0, -3.5, -1.8, -0.7, 0.7, 2.0, 5.0];

#[test]
fn grid_accuracy_within_0_1_degrees() {
    for expected in EXPECTED {
        // recommended = -phi なので、期待値 expected の画像は phi = -expected。
        let img = grid_image(640, 480, -expected, 90.0, true);
        let d = detect_tilt(&img, &params(1024));
        describe(&format!("grid {expected:+.1}"), &d);
        let got = d
            .recommended_angle_degrees
            .unwrap_or_else(|| panic!("no angle for {expected}: {d:?}"));
        assert!(
            (got - expected).abs() <= 0.1,
            "grid {expected}: got {got} ({d:?})"
        );
        assert!(d.confidence >= 0.5, "grid {expected}: low confidence {d:?}");
        assert_eq!(d.method, "hough_projection_fused");
        assert!(!d.alternatives.is_empty());
    }
}

#[test]
fn horizon_only_accuracy() {
    for expected in [-3.5, -0.7, 2.0, 5.0] {
        let img = horizon_image(640, 480, -expected);
        let d = detect_tilt(&img, &params(1024));
        describe(&format!("horizon {expected:+.1}"), &d);
        let got = d
            .recommended_angle_degrees
            .unwrap_or_else(|| panic!("no angle for {expected}: {d:?}"));
        assert!(
            (got - expected).abs() <= 0.1,
            "horizon {expected}: got {got} ({d:?})"
        );
        assert!(d.confidence >= 0.5, "horizon {expected}: {d:?}");
    }
}

/// 垂直線のみでも同じロール角が得られること(建築の柱が根拠になる)。
#[test]
fn verticals_alone_give_same_roll() {
    // 垂直方向の縞だけを持つ画像 = 格子を 90° ずらした形。
    let phi = 2.5;
    let img = grid_image(640, 480, phi + 90.0, 90.0, false);
    let d = detect_tilt(&img, &params(1024));
    describe("verticals-only +2.5phi", &d);
    let got = d.recommended_angle_degrees.expect("angle");
    assert!((got - -phi).abs() <= 0.1, "got {got} ({d:?})");
}

/// 符号の規約: 右下がりの水平線(x が増えると y が増える = 画面上で時計回り)
/// は「反時計回りに戻す」= 負の recommended_angle_degrees。
#[test]
fn sign_convention_clockwise_scene_needs_negative_angle() {
    let img = grid_image(640, 480, 4.0, 90.0, true);
    let d = detect_tilt(&img, &params(1024));
    describe("sign +4deg clockwise scene", &d);
    let got = d.recommended_angle_degrees.expect("angle");
    assert!(got < 0.0, "expected negative correction, got {got}");
    assert!((got - -4.0).abs() <= 0.1, "got {got}");
}

#[test]
fn level_image_reports_zero() {
    let img = grid_image(640, 480, 0.0, 90.0, true);
    let d = detect_tilt(&img, &params(1024));
    describe("level", &d);
    let got = d.recommended_angle_degrees.expect("angle");
    assert!(got.abs() <= 0.2, "got {got} ({d:?})");
}

#[test]
fn uniform_image_is_undetectable() {
    let img = DynamicImage::ImageLuma8(GrayImage::from_pixel(640, 480, Luma([128])));
    let d = detect_tilt(&img, &params(1024));
    describe("uniform", &d);
    assert_eq!(d.recommended_angle_degrees, None);
    assert!(d.confidence < 0.5);
    assert!(!d.warnings.is_empty());
}

#[test]
fn noise_image_is_undetectable() {
    let mut img = GrayImage::new(640, 480);
    let mut state: u32 = 0x1234_5678;
    for p in img.pixels_mut() {
        // xorshift32(決定論的)
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *p = Luma([(state >> 8) as u8]);
    }
    let d = detect_tilt(&DynamicImage::ImageLuma8(img), &params(1024));
    describe("noise", &d);
    assert_eq!(d.recommended_angle_degrees, None);
    assert!(d.confidence < 0.5, "{d:?}");
}

/// 斜め(45°)構造だけの画像は max_abs_angle 外なので棄却されること。
#[test]
fn diagonal_only_is_undetectable() {
    let img = grid_image(640, 480, 45.0, 90.0, false);
    let d = detect_tilt(&img, &params(1024));
    describe("diagonal45", &d);
    assert_eq!(d.recommended_angle_degrees, None, "{d:?}");
}

#[test]
fn deterministic_for_same_input() {
    let img = grid_image(640, 480, -1.8, 90.0, true);
    let a = detect_tilt(&img, &params(1024));
    let b = detect_tilt(&img, &params(1024));
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
}

/// 写真らしい合成フィクスチャ(`cargo run -p atx-core --example gen_fixture`)。
/// 建物の水平・垂直エッジが支配的で、シーン自体は完全に水平なので
/// 検出角はほぼ 0° になる(または棄却)。
#[test]
fn photo_like_fixture_is_level() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/synthetic_scene.jpg"
    );
    let img = image::open(path).expect("fixture");
    println!("fixture {}x{}", img.width(), img.height());
    let start = std::time::Instant::now();
    // debug ビルドでの実行時間を抑えるため作業解像度は 512。
    let d = detect_tilt(&img, &params(512));
    let elapsed = start.elapsed();
    describe("synthetic_scene", &d);
    println!("elapsed: {elapsed:?}");
    assert!(elapsed.as_secs() < 30, "too slow: {elapsed:?}");
    match d.recommended_angle_degrees {
        // シーンは完全に水平なので検出角はほぼ 0。
        // 実測値は +0.41°(水平族 +1.3° / 垂直族 +0.2° の融合)。1.0° で上限を張る。
        Some(a) => assert!(a.abs() < 1.0, "level scene but got {a} ({d:?})"),
        None => {
            // 棄却でも「候補と信頼度は返す」ことを要求する。
            assert!(!d.warnings.is_empty(), "{d:?}");
        }
    }
    assert!(d.confidence >= 0.0 && d.confidence <= 1.0);
}

/// `imageproc` の回転と符号規約が整合することを確認する
/// (atx-core の `Rotate` op は同じ回転関数を使う)。
///
/// `Projection::rotate(theta)` は (1,0) を (cos, sin) に写す。画像座標は y が
/// 下向きなので、これは画面上の**時計回り**回転。水平な被写体を +theta 回すと
/// 時計回りに傾くので、必要な補正角(正 = 時計回り)は -theta になる。
#[test]
fn imageproc_rotation_round_trip_matches_sign_convention() {
    use imageproc::geometric_transformations::{rotate_about_center, Border, Interpolation};

    for theta_deg in [-3.0f32, 3.0] {
        let level = grid_image(900, 900, 0.0, 90.0, true).to_luma8();
        let rotated = rotate_about_center(
            &level,
            theta_deg.to_radians(),
            Interpolation::Bilinear,
            Border::Constant(Luma([0u8])),
        );
        // 黒縁(回転で生じる三角形)を除くため中央 70% を切り出す。
        let (w, h) = (rotated.width(), rotated.height());
        let (cw, ch) = (w * 7 / 10, h * 7 / 10);
        let cropped =
            image::imageops::crop_imm(&rotated, (w - cw) / 2, (h - ch) / 2, cw, ch).to_image();
        let d = detect_tilt(&DynamicImage::ImageLuma8(cropped), &params(1024));
        describe(&format!("imageproc-rotated {theta_deg:+.1}"), &d);
        let got = d.recommended_angle_degrees.expect("angle");
        let expected = -theta_deg as f64;
        assert!(
            (got - expected).abs() <= 0.1,
            "rotated {theta_deg}: got {got}, expected {expected} ({d:?})"
        );
    }
}

// ---------------------------------------------------------------------------
// 投影プロファイル法(短く途切れたエッジ)・水平/垂直の分離・スコア曲線
// ---------------------------------------------------------------------------

/// 破線・短いセグメント + ノイズでも既知角を 1:1 で追えること(誤差 ≤ 0.1°)。
#[test]
fn dashed_segments_track_known_angles_within_0_1_degrees() {
    for expected in [-5.0, -2.0, -0.9, -0.5, -0.2, 0.3, 1.7, 4.0] {
        // recommended = -phi なので、期待値 expected の画像は phi = -expected。
        let img = dashed_image(800, 600, -expected, Some(-expected));
        let d = detect_tilt(&img, &params(1024));
        describe(&format!("dashed {expected:+.2}"), &d);
        let got = d
            .recommended_angle_degrees
            .unwrap_or_else(|| panic!("no angle for {expected}: {d:?}"));
        assert!(
            (got - expected).abs() <= 0.1,
            "dashed {expected}: got {got} ({d:?})"
        );
        // 族ごとの推定も同じ角を指すこと。
        for (name, fam) in [
            ("horizontal", d.horizontal_angle_degrees),
            ("vertical", d.vertical_angle_degrees),
        ] {
            let a = fam.unwrap_or_else(|| panic!("no {name} estimate for {expected}: {d:?}"));
            assert!(
                (a - expected).abs() <= 0.1,
                "dashed {expected} {name}: got {a} ({d:?})"
            );
        }
        assert!(d.confidence >= 0.5, "dashed {expected}: {d:?}");
    }
}

/// 水平線だけが -0.5° 傾き、垂直線は完全に垂直なシーン。
///
/// 現場の要望: 「水平は -0.5°、垂直は 0°」を**別々に**返せれば、
/// ロール(カメラの傾き)ではなくカメラ位置・パースの問題だと判断できる。
#[test]
fn horizontal_and_vertical_families_are_reported_separately() {
    // phi_h = +0.5 (= 補正角 -0.5)、phi_v = 0(垂直は完全に立っている)。
    let img = dashed_image(800, 600, 0.5, Some(0.0));
    let d = detect_tilt(&img, &params(1024));
    describe("h=-0.5 / v=0.0", &d);

    let h = d.horizontal_angle_degrees.expect("horizontal estimate");
    let v = d.vertical_angle_degrees.expect("vertical estimate");
    assert!((h - -0.5).abs() <= 0.1, "horizontal: got {h} ({d:?})");
    assert!(v.abs() <= 0.1, "vertical: got {v} ({d:?})");
    // 両族とも十分な根拠を持ち、支持の合計は 1。
    assert!(d.horizontal_confidence >= 0.5, "{d:?}");
    assert!(d.vertical_confidence >= 0.5, "{d:?}");
    assert!(
        (d.horizontal_support + d.vertical_support - 1.0).abs() <= 0.01,
        "{d:?}"
    );
    // 食い違いは警告としても見えること。
    assert!(
        d.warnings.iter().any(|w| w.contains("perspective")),
        "expected an H/V disagreement warning: {d:?}"
    );
    // 融合値は両者の間に入る(どちらか一方に飛びつかない)。
    let rec = d.recommended_angle_degrees.expect("angle");
    assert!((-0.55..=0.05).contains(&rec), "fused angle {rec} ({d:?})");
}

/// 完全に垂直な線しかない画像では、水平族は「何も言えない」= None になること。
#[test]
fn family_without_evidence_is_none() {
    let img = grid_image(640, 480, 90.0, 90.0, false);
    let d = detect_tilt(&img, &params(1024));
    describe("verticals only", &d);
    assert_eq!(d.horizontal_angle_degrees, None, "{d:?}");
    assert_eq!(d.horizontal_confidence, 0.0);
    assert_eq!(d.horizontal_support, 0.0);
    let v = d.vertical_angle_degrees.expect("vertical estimate");
    assert!(v.abs() <= 0.1, "vertical: got {v} ({d:?})");
}

/// スコア曲線の健全性: 点数上限・正規化・昇順・推奨角にピーク。
#[test]
fn score_curve_is_compact_and_peaks_at_the_recommended_angle() {
    let img = dashed_image(800, 600, 1.2, Some(1.2));
    let d = detect_tilt(&img, &params(1024));
    describe("score curve", &d);
    let curve = &d.score_curve;

    assert!(!curve.is_empty(), "{d:?}");
    assert!(curve.len() <= 300, "curve too large: {}", curve.len());
    // 補正角の昇順、探索範囲内、score は 0..=1。
    for w in curve.windows(2) {
        assert!(
            w[0].angle_degrees < w[1].angle_degrees,
            "not sorted: {:?} {:?}",
            w[0],
            w[1]
        );
    }
    for p in curve {
        assert!(p.angle_degrees.abs() <= 15.0 + 1e-9, "{p:?}");
        assert!((0.0..=1.0).contains(&p.score), "{p:?}");
    }
    // 最大値は 1.0 ちょうどで、その位置が推奨角。
    let peak = curve
        .iter()
        .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap())
        .unwrap();
    assert_eq!(peak.score, 1.0);
    let rec = d.recommended_angle_degrees.expect("angle");
    assert_eq!(
        peak.angle_degrees, rec,
        "peak {peak:?} vs recommended {rec}"
    );
    // 端は明確に低い(ピークが立っている)。
    assert!(curve[0].score < 0.5, "{:?}", curve[0]);
    assert!(curve[curve.len() - 1].score < 0.5, "{:?}", curve.last());
}

#[test]
fn low_min_confidence_still_reports_alternatives() {
    let img = grid_image(640, 480, -1.8, 90.0, true);
    let strict = DetectParams {
        min_confidence: 1.01,
        ..params(1024)
    };
    let d = detect_tilt(&img, &strict);
    assert_eq!(d.recommended_angle_degrees, None);
    assert!(!d.alternatives.is_empty());
    assert!(d.confidence > 0.5);
}
