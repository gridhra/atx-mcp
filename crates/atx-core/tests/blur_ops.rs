//! blur / median / unsharp_mask のテスト(v0.2)。
//!
//! `apply_recipe` を通した end-to-end 検証を基本とする(recipe.rs の validate +
//! engine.rs の dispatch + ops/blur.rs の画素演算を一括で回帰させるため)。

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe, Limits};
use image::{Rgba, RgbaImage};

const FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");

fn recipe(json: &str) -> TransformRecipe {
    serde_json::from_str(json).expect("recipe should parse")
}

fn apply(bytes: &[u8], json: &str) -> atx_core::EncodedOutput {
    apply_recipe(bytes, &recipe(json), &Limits::default()).expect("apply_recipe should succeed")
}

fn apply_err(bytes: &[u8], json: &str) -> String {
    apply_recipe(bytes, &recipe(json), &Limits::default())
        .unwrap_err()
        .to_string()
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn decode_rgba(bytes: &[u8]) -> RgbaImage {
    image::load_from_memory(bytes)
        .expect("output should decode")
        .to_rgba8()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// 一様な色の画像(ぼかしても不変であることの確認用)。
fn uniform(w: u32, h: u32, color: [u8; 4]) -> RgbaImage {
    RgbaImage::from_pixel(w, h, Rgba(color))
}

// ---------------------------------------------------------------------------
// 決定論
// ---------------------------------------------------------------------------

fn assert_deterministic(input: &[u8], json: &str) -> Vec<u8> {
    let a = apply(input, json);
    let b = apply(input, json);
    assert_eq!(a.bytes, b.bytes, "output bytes must be identical: {json}");
    assert_eq!((a.width, a.height), (b.width, b.height));
    a.bytes
}

#[test]
fn blur_is_deterministic() {
    let input = encode_png(&uniform(64, 48, [10, 20, 30, 255]));
    assert_deterministic(
        &input,
        r#"{"operations":[{"op":"blur","sigma":3.5},{"op":"encode","format":"png"}]}"#,
    );
}

#[test]
fn median_is_deterministic() {
    let input = encode_png(&uniform(64, 48, [10, 20, 30, 255]));
    assert_deterministic(
        &input,
        r#"{"operations":[{"op":"median","radius":3},{"op":"encode","format":"png"}]}"#,
    );
}

#[test]
fn unsharp_mask_is_deterministic() {
    let input = encode_png(&uniform(64, 48, [10, 20, 30, 255]));
    assert_deterministic(
        &input,
        r#"{"operations":[
            {"op":"unsharp_mask","amount":1.5,"radius":2.0,"threshold":4},
            {"op":"encode","format":"png"}
        ]}"#,
    );
}

// ---------------------------------------------------------------------------
// gaussian blur
// ---------------------------------------------------------------------------

/// 一様画像はぼかしても不変(カーネルは量子化後に厳密に 1.0 へ正規化されるため、
/// 丸め後も完全一致する)。
#[test]
fn blur_uniform_image_is_unchanged() {
    let color = [37, 88, 200, 255];
    let input = encode_png(&uniform(40, 30, color));
    let out = apply(
        &input,
        r#"{"operations":[{"op":"blur","sigma":8.0},{"op":"encode","format":"png"}]}"#,
    );
    let decoded = decode_rgba(&out.bytes);
    for p in decoded.pixels() {
        assert_eq!(
            p.0, color,
            "uniform image must be exactly unchanged by blur"
        );
    }
}

/// 黒地に白1画素のインパルス応答は分離可能ガウスカーネルにより点対称
/// (上下左右に4回対称)になる。
#[test]
fn blur_impulse_response_is_symmetric() {
    let size = 41u32;
    let center = (size / 2) as i32;
    let mut img = uniform(size, size, [0, 0, 0, 255]);
    img.put_pixel(center as u32, center as u32, Rgba([255, 255, 255, 255]));
    let input = encode_png(&img);
    let out = apply(
        &input,
        r#"{"operations":[{"op":"blur","sigma":4.0},{"op":"encode","format":"png"}]}"#,
    );
    let decoded = decode_rgba(&out.bytes);

    for dx in 0..=10i32 {
        for dy in 0..=10i32 {
            let base = decoded
                .get_pixel((center + dx) as u32, (center + dy) as u32)
                .0;
            for (sx, sy) in [(dx, -dy), (-dx, dy), (-dx, -dy)] {
                let px = decoded
                    .get_pixel((center + sx) as u32, (center + sy) as u32)
                    .0;
                assert_eq!(
                    px, base,
                    "impulse response must be 4-fold symmetric at ({dx},{dy}) vs ({sx},{sy})"
                );
            }
        }
    }
}

/// 大きい sigma ほど画素値の分散(=尖度)が下がる、という統計的な単調性。
#[test]
fn blur_larger_sigma_reduces_variance() {
    // 高コントラストな市松模様(分散が大きい入力)。
    let img = RgbaImage::from_fn(80, 80, |x, y| {
        let v = if (x / 4 + y / 4) % 2 == 0 { 20 } else { 235 };
        Rgba([v, v, v, 255])
    });
    let input = encode_png(&img);

    let variance_of = |sigma: f64| -> f64 {
        let out = apply(
            &input,
            &format!(
                r#"{{"operations":[{{"op":"blur","sigma":{sigma}}},{{"op":"encode","format":"png"}}]}}"#
            ),
        );
        let decoded = decode_rgba(&out.bytes);
        let vals: Vec<f64> = decoded.pixels().map(|p| p.0[0] as f64).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64
    };

    // sigma を画像サイズに対して大きくしすぎると、クランプ境界の複製が
    // 支配的になり分散が反転して増加する(境界アーティファクト、バグではない)。
    // ここでは境界の影響が小さい範囲の sigma で単調性を確認する。
    let var_small = variance_of(0.8);
    let var_medium = variance_of(2.0);
    let var_large = variance_of(4.0);
    assert!(
        var_small > var_medium && var_medium > var_large,
        "variance should strictly decrease as sigma grows: {var_small} > {var_medium} > {var_large}"
    );
}

/// 性能サニティ: 実寸フィクスチャ(1477x1108)に sigma=100(上限付近)の
/// ガウスぼかしをかけても、分離実装であれば妥当な時間で完了する
/// (ナイーブな2D畳み込みでは半径255の正方形窓 = 実運用不可能な計算量になる)。
#[test]
fn blur_large_sigma_on_full_fixture_completes_quickly() {
    let start = std::time::Instant::now();
    let out = apply(
        FIXTURE,
        r#"{"operations":[{"op":"blur","sigma":100.0},{"op":"encode","format":"png"}]}"#,
    );
    let elapsed = start.elapsed();
    assert!(!out.bytes.is_empty());
    // 素朴な 2D 実装(数分〜)を検出するためのカナリア。分離可能 + 行並列実装なら
    // 手元 arm64 で ~20s、CI の低コア ubuntu ランナーで ~66s(debug ビルド)。
    // ランナー性能差で偽陽性にならないよう余裕を持たせた上限にする。
    assert!(
        elapsed.as_secs() < 150,
        "separable gaussian blur at sigma=100 on the full fixture took too long: {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// median
// ---------------------------------------------------------------------------

#[test]
fn median_of_uniform_image_is_identical() {
    let color = [12, 34, 56, 255];
    let input = encode_png(&uniform(30, 20, color));
    let out = apply(
        &input,
        r#"{"operations":[{"op":"median","radius":4},{"op":"encode","format":"png"}]}"#,
    );
    let decoded = decode_rgba(&out.bytes);
    for p in decoded.pixels() {
        assert_eq!(p.0, color);
    }
}

/// 塩胡椒ノイズ(ごく少数の孤立した極値画素)はメディアンフィルタで除去される。
#[test]
fn median_removes_salt_and_pepper_noise() {
    let base = [128, 128, 128, 255];
    let mut img = uniform(40, 40, base);
    // 孤立ノイズ画素を格子状に散らす(隣接しないよう間隔をあける)。
    let noisy_pixels = [(10, 10), (20, 20), (30, 30), (15, 25)];
    for (i, &(x, y)) in noisy_pixels.iter().enumerate() {
        let v = if i % 2 == 0 { 255 } else { 0 };
        img.put_pixel(x, y, Rgba([v, v, v, 255]));
    }
    let input = encode_png(&img);
    let out = apply(
        &input,
        r#"{"operations":[{"op":"median","radius":2},{"op":"encode","format":"png"}]}"#,
    );
    let decoded = decode_rgba(&out.bytes);
    for &(x, y) in &noisy_pixels {
        let px = decoded.get_pixel(x, y).0;
        assert_eq!(
            px, base,
            "isolated noise pixel at ({x},{y}) should be replaced by the surrounding median"
        );
    }
}

// ---------------------------------------------------------------------------
// unsharp_mask
// ---------------------------------------------------------------------------

/// amount=0 なら入力バイト列と完全一致する(ブレンド式が orig + 0*diff = orig のため)。
#[test]
fn unsharp_mask_amount_zero_is_byte_identical_to_source() {
    let img = RgbaImage::from_fn(50, 40, |x, y| {
        Rgba([(x * 5) as u8, (y * 5) as u8, 100, 255])
    });
    let input = encode_png(&img);
    let out = apply(
        &input,
        r#"{"operations":[
            {"op":"unsharp_mask","amount":0.0,"radius":3.0,"threshold":0},
            {"op":"encode","format":"png"}
        ]}"#,
    );
    assert_eq!(out.bytes, input, "amount=0 must be a byte-identical no-op");
}

/// ステップエッジ(段差)を挟んだコントラストは unsharp mask で強調される
/// (明るい側はより明るく、暗い側はより暗くなる)。
#[test]
fn unsharp_mask_increases_contrast_across_step_edge() {
    let w = 60u32;
    let h = 20u32;
    let img = RgbaImage::from_fn(w, h, |x, _y| {
        let v = if x < w / 2 { 80 } else { 180 };
        Rgba([v, v, v, 255])
    });
    let input = encode_png(&img);
    let out = apply(
        &input,
        r#"{"operations":[
            {"op":"unsharp_mask","amount":2.0,"radius":3.0,"threshold":0},
            {"op":"encode","format":"png"}
        ]}"#,
    );
    let decoded = decode_rgba(&out.bytes);

    // エッジ直前(暗い側)は元の 80 よりさらに暗く、
    // エッジ直後(明るい側)は元の 180 よりさらに明るくなるはず。
    let dark_probe = decoded.get_pixel(w / 2 - 2, 10).0[0];
    let bright_probe = decoded.get_pixel(w / 2 + 1, 10).0[0];
    assert!(
        dark_probe < 80,
        "dark side of the edge should be pushed darker, got {dark_probe}"
    );
    assert!(
        bright_probe > 180,
        "bright side of the edge should be pushed brighter, got {bright_probe}"
    );
}

/// threshold を超えないフラットな微小ノイズは保護され、変化しない。
#[test]
fn unsharp_mask_threshold_protects_flat_noise() {
    let base = 128u8;
    // ±3 の微小ノイズを市松状に配置(threshold=6 より小さい差)。
    let img = RgbaImage::from_fn(30, 30, |x, y| {
        let v = if (x + y) % 2 == 0 { base + 3 } else { base - 3 };
        Rgba([v, v, v, 255])
    });
    let input = encode_png(&img);
    let out = apply(
        &input,
        r#"{"operations":[
            {"op":"unsharp_mask","amount":3.0,"radius":4.0,"threshold":6},
            {"op":"encode","format":"png"}
        ]}"#,
    );
    assert_eq!(
        out.bytes, input,
        "flat noise within the threshold must be left untouched"
    );
}

// ---------------------------------------------------------------------------
// validate 経由の拒否(apply_recipe → InvalidRecipe、op index を含む)
// ---------------------------------------------------------------------------

#[test]
fn blur_rejects_out_of_range_sigma() {
    let input = encode_png(&uniform(8, 8, [1, 2, 3, 255]));
    let msg = apply_err(&input, r#"{"operations":[{"op":"blur","sigma":0.05}]}"#);
    assert!(msg.contains("operations[0]"), "{msg}");
    assert!(msg.contains("blur"), "{msg}");

    let msg = apply_err(&input, r#"{"operations":[{"op":"blur","sigma":100.1}]}"#);
    assert!(msg.contains("operations[0]"), "{msg}");
}

#[test]
fn median_rejects_out_of_range_radius() {
    let input = encode_png(&uniform(8, 8, [1, 2, 3, 255]));
    let msg = apply_err(&input, r#"{"operations":[{"op":"median","radius":0}]}"#);
    assert!(msg.contains("operations[0]"), "{msg}");
    assert!(msg.contains("median"), "{msg}");

    let msg = apply_err(&input, r#"{"operations":[{"op":"median","radius":17}]}"#);
    assert!(msg.contains("operations[0]"), "{msg}");
}

#[test]
fn unsharp_mask_rejects_out_of_range_amount_and_radius() {
    let input = encode_png(&uniform(8, 8, [1, 2, 3, 255]));

    let msg = apply_err(
        &input,
        r#"{"operations":[{"op":"unsharp_mask","amount":4.1,"radius":2.0,"threshold":0}]}"#,
    );
    assert!(msg.contains("operations[0]"), "{msg}");
    assert!(msg.contains("unsharp_mask"), "{msg}");

    let msg = apply_err(
        &input,
        r#"{"operations":[{"op":"unsharp_mask","amount":1.0,"radius":0.0,"threshold":0}]}"#,
    );
    assert!(msg.contains("operations[0]"), "{msg}");

    let msg = apply_err(
        &input,
        r#"{"operations":[{"op":"unsharp_mask","amount":1.0,"radius":50.1,"threshold":0}]}"#,
    );
    assert!(msg.contains("operations[0]"), "{msg}");
}

#[test]
fn rejection_reports_correct_op_index_when_not_first() {
    let input = encode_png(&uniform(16, 16, [1, 2, 3, 255]));
    let msg = apply_err(
        &input,
        r#"{"operations":[
            {"op":"median","radius":2},
            {"op":"blur","sigma":200.0}
        ]}"#,
    );
    assert!(msg.contains("operations[1]"), "{msg}");
    assert!(msg.contains("blur"), "{msg}");
}

// ---------------------------------------------------------------------------
// フルパイプラインのゴールデン(sha256)
// ---------------------------------------------------------------------------

/// blur + unsharp_mask + jpeg encode の一気通貫ゴールデン。
///
/// このハッシュは `tests/fixtures/synthetic_scene.jpg` の内容、
/// gaussian_blur のカーネル量子化規則、unsharp_mask のブレンド式、
/// および jpeg エンコーダの挙動に依存する。フィクスチャや量子化規則、
/// エンコーダのバージョンを変えたら値を更新すること
/// (`ENGINE_VERSION` を上げるべき変更かどうかも合わせて検討する)。
#[test]
fn full_pipeline_golden_blur_unsharp_jpeg() {
    let out = apply(
        FIXTURE,
        r#"{"operations":[
            {"op":"auto_orient"},
            {"op":"blur","sigma":2.5},
            {"op":"unsharp_mask","amount":1.2,"radius":3.0,"threshold":2},
            {"op":"encode","format":"jpeg","quality":90}
        ]}"#,
    );
    let hash = sha256_hex(&out.bytes);
    assert_eq!(
        hash, // v2 (f32 linear) golden; v1 value was 9f93db6234def3510714674c69de34b5fd2398e7d2d97540265d42fce6beedc9
        "08c0775c1a40288d40e31ff4268126e033935ab1aa048e7ff17a28455d017fc4",
        "golden hash mismatch: full pipeline output changed (recompute and update if intentional)"
    );
}
