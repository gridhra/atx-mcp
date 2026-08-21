//! convolve のテスト(v0.3)。
//!
//! `apply_recipe` を通した end-to-end 検証を基本とする(recipe.rs の validate +
//! engine.rs の dispatch + ops/convolve.rs の画素演算を一括で回帰させるため)。

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

// ---------------------------------------------------------------------------
// v2(f32 リニアライト)の期待値ヘルパ
//
// convolve は **線形光**で畳み込む op になったため(ops/mod.rs の作業空間表)、
// 期待値も「sRGB を線形化 → 線形で重み付き和 → sRGB へ戻す」で立てる。
// ---------------------------------------------------------------------------

/// sRGB EOTF(符号値 0..1 → 線形光)。
fn eotf(c: f64) -> f64 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB OETF(線形光 → 符号値 0..1)。
fn oetf(l: f64) -> f64 {
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// u8 符号値 → 線形光。
fn lin(v: u8) -> f64 {
    eotf(v as f64 / 255.0)
}

/// 線形光 → u8 符号値(half-away-from-zero)。
fn srgb_u8(l: f64) -> u8 {
    (oetf(l.clamp(0.0, 1.0)) * 255.0).round().clamp(0.0, 255.0) as u8
}

/// 一様な色の画像。
fn uniform(w: u32, h: u32, color: [u8; 4]) -> RgbaImage {
    RgbaImage::from_pixel(w, h, Rgba(color))
}

fn identity_kernel_json() -> &'static str {
    r#"[0,0,0,0,1,0,0,0,0]"#
}

fn box3_kernel_json() -> &'static str {
    r#"[1,1,1,1,1,1,1,1,1]"#
}

// ---------------------------------------------------------------------------
// identity / box blur
// ---------------------------------------------------------------------------

/// 恒等カーネル(中心のみ 1、他 0、divisor=1、offset=0)はバイト同一出力になる。
#[test]
fn identity_kernel_is_byte_identical() {
    let img = RgbaImage::from_fn(37, 29, |x, y| {
        Rgba([(x * 3) as u8, (y * 5) as u8, ((x + y) * 2) as u8, 200])
    });
    let input = encode_png(&img);
    let out = apply(
        &input,
        &format!(
            r#"{{"operations":[
                {{"op":"convolve","kernel":{},"size":3,"divisor":1.0,"offset":0.0}},
                {{"op":"encode","format":"png"}}
            ]}}"#,
            identity_kernel_json()
        ),
    );
    assert_eq!(
        out.bytes, input,
        "identity kernel must be a byte-identical no-op"
    );
}

/// 一様画像に box blur (全て1、divisor=9) をかけても不変。
#[test]
fn box_blur_uniform_image_is_unchanged() {
    let color = [40, 90, 210, 255];
    let input = encode_png(&uniform(20, 20, color));
    let out = apply(
        &input,
        &format!(
            r#"{{"operations":[
                {{"op":"convolve","kernel":{},"size":3,"divisor":9.0,"offset":0.0}},
                {{"op":"encode","format":"png"}}
            ]}}"#,
            box3_kernel_json()
        ),
    );
    let decoded = decode_rgba(&out.bytes);
    for p in decoded.pixels() {
        assert_eq!(p.0, color, "uniform image must be unchanged by box blur");
    }
}

// ---------------------------------------------------------------------------
// sharpen / emboss
// ---------------------------------------------------------------------------

/// シャープンカーネルはステップエッジのコントラストを強調する。
#[test]
fn sharpen_kernel_increases_step_edge_contrast() {
    let w = 60u32;
    let h = 20u32;
    let img = RgbaImage::from_fn(w, h, |x, _y| {
        let v = if x < w / 2 { 80 } else { 180 };
        Rgba([v, v, v, 255])
    });
    let input = encode_png(&img);
    let sharpen = r#"[0,-1,0,-1,5,-1,0,-1,0]"#;
    let out = apply(
        &input,
        &format!(
            r#"{{"operations":[
                {{"op":"convolve","kernel":{sharpen},"size":3,"divisor":1.0,"offset":0.0}},
                {{"op":"encode","format":"png"}}
            ]}}"#
        ),
    );
    let decoded = decode_rgba(&out.bytes);

    // 3x3 カーネルなので影響が及ぶのは境界の直近1画素のみ。
    let dark_probe = decoded.get_pixel(w / 2 - 1, 10).0[0];
    let bright_probe = decoded.get_pixel(w / 2, 10).0[0];
    assert!(
        dark_probe < 80,
        "dark side of the edge should be pushed darker, got {dark_probe}"
    );
    assert!(
        bright_probe > 180,
        "bright side of the edge should be pushed brighter, got {bright_probe}"
    );
}

/// エンボスカーネル + offset=128 は、一様(フラット)領域では **線形光の 128/255** になる。
///
/// 一様領域ではカーネル係数の和が 0(ゼロサム)なので `acc = 0`、
/// 出力は `0/divisor + offset` = offset そのものになる。
/// **v2 の変更点**: 画素は線形光の 0..1 で、`offset` は u8 スケールのまま
/// 受け取って `offset/255` を線形光に足す(ops/convolve.rs のドキュメント参照)。
/// したがって出力は「線形光 128/255 = 0.50196」であり、
/// sRGB 符号値に戻すと 188 になる(v1 は符号値 128 だった)。
/// 「符号値としての中間グレー」に寄せたい場合は offset を 55 前後にする。
#[test]
fn emboss_kernel_with_offset_produces_exact_flat_value() {
    let color = [77, 150, 33, 255];
    let input = encode_png(&uniform(30, 30, color));
    let emboss = r#"[-1,-1,0,-1,0,1,0,1,1]"#;
    let out = apply(
        &input,
        &format!(
            r#"{{"operations":[
                {{"op":"convolve","kernel":{emboss},"size":3,"divisor":1.0,"offset":128.0}},
                {{"op":"encode","format":"png"}}
            ]}}"#
        ),
    );
    let decoded = decode_rgba(&out.bytes);
    let expected = srgb_u8(128.0 / 255.0);
    assert_eq!(expected, 188, "linear-light 128/255 encodes to sRGB 188");
    for p in decoded.pixels() {
        assert_eq!(
            &p.0[0..3],
            &[expected, expected, expected],
            "flat region must map to exactly offset (in linear light)"
        );
        assert_eq!(p.0[3], 255, "alpha untouched");
    }
}

// ---------------------------------------------------------------------------
// alpha / edge behavior
// ---------------------------------------------------------------------------

/// アルファチャンネルは畳み込み対象外(半透明入力でも alpha バイトは不変)。
#[test]
fn alpha_channel_is_untouched() {
    let img = RgbaImage::from_fn(25, 25, |x, y| {
        Rgba([(x * 7) as u8, (y * 7) as u8, 100, ((x + y) % 200) as u8])
    });
    let input = encode_png(&img);
    let sharpen = r#"[0,-1,0,-1,5,-1,0,-1,0]"#;
    let out = apply(
        &input,
        &format!(
            r#"{{"operations":[
                {{"op":"convolve","kernel":{sharpen},"size":3,"divisor":1.0,"offset":0.0}},
                {{"op":"encode","format":"png"}}
            ]}}"#
        ),
    );
    let decoded = decode_rgba(&out.bytes);
    for y in 0..25 {
        for x in 0..25 {
            let expected_alpha = img.get_pixel(x, y).0[3];
            let actual_alpha = decoded.get_pixel(x, y).0[3];
            assert_eq!(
                actual_alpha, expected_alpha,
                "alpha must be exactly preserved at ({x},{y})"
            );
        }
    }
}

/// 端の画素はクランプ(複製)境界で扱われる。左上隅コーナーに 3x3 の
/// box blur (divisor=9) をかけると、複製された 3x3 = 9 サンプルの平均になる。
#[test]
fn corner_edge_uses_clamp_replicate_border() {
    // 左上4画素だけ異なる色、それ以外は背景色。
    let bg = [0u8, 0, 0, 255];
    let corner = [90u8, 180, 30, 255];
    let mut img = uniform(10, 10, bg);
    img.put_pixel(0, 0, Rgba(corner));
    let input = encode_png(&img);
    let out = apply(
        &input,
        &format!(
            r#"{{"operations":[
                {{"op":"convolve","kernel":{},"size":3,"divisor":9.0,"offset":0.0}},
                {{"op":"encode","format":"png"}}
            ]}}"#,
            box3_kernel_json()
        ),
    );
    let decoded = decode_rgba(&out.bytes);
    // (0,0) の 3x3 窓(クランプ後)は corner が4回(自身+複製2方向+対角複製)、
    // bg が5回サンプルされる: 窓は {(-1,-1)...(1,1)} クランプ後 {(0,0)x4,(0,1)x2,(1,0)x2,(1,1)}
    // = corner:4, bg:5。
    //
    // **v2 の変更点**: 平均は線形光で取る。暗い bg(=0)と明るい corner の平均は
    // 符号値平均より明るくなる(符号値平均 [40,80,13] → 線形平均 [60,124,17])。
    // これは「暗部に寄る」ガンマ・ブラー誤差が解消された結果であり、
    // 物理的に正しいのは線形平均のほう。
    let expected = [
        srgb_u8((lin(corner[0]) * 4.0 + lin(bg[0]) * 5.0) / 9.0),
        srgb_u8((lin(corner[1]) * 4.0 + lin(bg[1]) * 5.0) / 9.0),
        srgb_u8((lin(corner[2]) * 4.0 + lin(bg[2]) * 5.0) / 9.0),
    ];
    let actual = decoded.get_pixel(0, 0).0;
    assert_eq!(
        &actual[0..3],
        &expected[..],
        "corner clamp-averaged value mismatch"
    );
}

// ---------------------------------------------------------------------------
// 決定論
// ---------------------------------------------------------------------------

#[test]
fn convolve_is_deterministic() {
    let img = RgbaImage::from_fn(50, 40, |x, y| {
        Rgba([(x * 5) as u8, (y * 5) as u8, 100, 255])
    });
    let input = encode_png(&img);
    let sharpen = r#"[0,-1,0,-1,5,-1,0,-1,0]"#;
    let json = format!(
        r#"{{"operations":[
            {{"op":"convolve","kernel":{sharpen},"size":3,"divisor":1.0,"offset":0.0}},
            {{"op":"encode","format":"png"}}
        ]}}"#
    );
    let a = apply(&input, &json);
    let b = apply(&input, &json);
    assert_eq!(a.bytes, b.bytes, "output bytes must be identical");
    assert_eq!((a.width, a.height), (b.width, b.height));
}

// ---------------------------------------------------------------------------
// validate 経由の拒否
// ---------------------------------------------------------------------------

#[test]
fn convolve_rejects_bad_size() {
    let input = encode_png(&uniform(8, 8, [1, 2, 3, 255]));
    let msg = apply_err(
        &input,
        r#"{"operations":[{"op":"convolve","kernel":[1,1,1,1],"size":4,"divisor":1.0,"offset":0.0}]}"#,
    );
    assert!(msg.contains("operations[0]"), "{msg}");
    assert!(msg.contains("convolve"), "{msg}");
}

#[test]
fn convolve_rejects_wrong_kernel_length() {
    let input = encode_png(&uniform(8, 8, [1, 2, 3, 255]));
    let msg = apply_err(
        &input,
        r#"{"operations":[{"op":"convolve","kernel":[1,1,1,1],"size":3,"divisor":1.0,"offset":0.0}]}"#,
    );
    assert!(msg.contains("operations[0]"), "{msg}");
    assert!(msg.contains("convolve"), "{msg}");
}

#[test]
fn convolve_rejects_zero_divisor() {
    let input = encode_png(&uniform(8, 8, [1, 2, 3, 255]));
    let msg = apply_err(
        &input,
        &format!(
            r#"{{"operations":[{{"op":"convolve","kernel":{},"size":3,"divisor":0.0,"offset":0.0}}]}}"#,
            box3_kernel_json()
        ),
    );
    assert!(msg.contains("operations[0]"), "{msg}");
    assert!(msg.contains("convolve"), "{msg}");
}

/// f64::NAN / INFINITY は JSON リテラルとして書けないため、レシピを Rust 側で
/// 構築して `recipe::validate` を直接呼ぶことで非有限値の拒否を検証する。
#[test]
fn convolve_rejects_non_finite_kernel_value() {
    use atx_core::recipe::{Operation, TransformRecipe};

    let mut kernel = vec![0.0f64; 9];
    kernel[4] = f64::NAN;
    let recipe = TransformRecipe {
        layers: None,
        operations: vec![Operation::Convolve {
            kernel,
            size: 3,
            divisor: 1.0,
            offset: 0.0,
            mask: None,
        }],
    };
    let msg = atx_core::recipe::validate(&recipe).unwrap_err().to_string();
    assert!(msg.contains("operations[0]"), "{msg}");
    assert!(msg.contains("convolve"), "{msg}");
}

#[test]
fn convolve_rejects_kernel_value_too_large() {
    let input = encode_png(&uniform(8, 8, [1, 2, 3, 255]));
    let msg = apply_err(
        &input,
        r#"{"operations":[{"op":"convolve","kernel":[0,0,0,0,257.0,0,0,0,0],"size":3,"divisor":1.0,"offset":0.0}]}"#,
    );
    assert!(msg.contains("operations[0]"), "{msg}");
    assert!(msg.contains("convolve"), "{msg}");
}

#[test]
fn convolve_rejects_out_of_range_offset() {
    let input = encode_png(&uniform(8, 8, [1, 2, 3, 255]));
    let msg = apply_err(
        &input,
        &format!(
            r#"{{"operations":[{{"op":"convolve","kernel":{},"size":3,"divisor":1.0,"offset":300.0}}]}}"#,
            box3_kernel_json()
        ),
    );
    assert!(msg.contains("operations[0]"), "{msg}");
    assert!(msg.contains("convolve"), "{msg}");
}

// ---------------------------------------------------------------------------
// フルパイプラインのゴールデン(sha256)
// ---------------------------------------------------------------------------

/// sharpen 3x3 + jpeg encode の一気通貫ゴールデン。
///
/// このハッシュは `tests/fixtures/synthetic_scene.jpg` の内容、
/// ops/convolve.rs の累算順序・丸め規則、および jpeg エンコーダの挙動に依存する。
/// フィクスチャや量子化規則、エンコーダのバージョンを変えたら値を更新すること
/// (`ENGINE_VERSION` を上げるべき変更かどうかも合わせて検討する)。
#[test]
fn full_pipeline_golden_sharpen_jpeg() {
    let out = apply(
        FIXTURE,
        r#"{"operations":[
            {"op":"auto_orient"},
            {"op":"convolve","kernel":[0,-1,0,-1,5,-1,0,-1,0],"size":3,"divisor":1.0,"offset":0.0},
            {"op":"encode","format":"jpeg","quality":90}
        ]}"#,
    );
    let hash = sha256_hex(&out.bytes);
    assert_eq!(
        hash, // v2 (f32 linear) golden; v1 value was 7e22f7205979c3bb8ea15585faf75bda48139e78b67c65b798bfea06ee5ccd53
        "9be5fdfa2cab50fde4d4acb0810c7643d918c38b1d62e55f65ab4ce0993d2d3f",
        "golden hash mismatch: full pipeline output changed (recompute and update if intentional)"
    );
}

// ---------------------------------------------------------------------------
// 性能サニティ
// ---------------------------------------------------------------------------

/// 性能サニティ: 実寸フィクスチャ(1477x1108)に 9x9 カーネルをかけても
/// 妥当な時間で完了する(行並列化されていれば問題ない計算量)。
#[test]
fn convolve_9x9_on_full_fixture_completes_quickly() {
    let kernel: Vec<f64> = vec![1.0; 81];
    let kernel_json = serde_json::to_string(&kernel).unwrap();
    let start = std::time::Instant::now();
    let out = apply(
        FIXTURE,
        &format!(
            r#"{{"operations":[
                {{"op":"convolve","kernel":{kernel_json},"size":9,"divisor":81.0,"offset":0.0}},
                {{"op":"encode","format":"png"}}
            ]}}"#
        ),
    );
    let elapsed = start.elapsed();
    assert!(!out.bytes.is_empty());
    assert!(
        elapsed.as_secs() < 60,
        "9x9 convolve on the full fixture took too long: {elapsed:?}"
    );
}
