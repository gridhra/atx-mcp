//! flip / vignette / grain(v0.3 仕上げ系 op)のテスト。
//!
//! すべて `apply_recipe`(JSON レシピ)経由で実行する。ユニット呼び出しではなく
//! エンジンのディスパッチ(`op_space`: flip=空間非依存 / vignette=線形光 /
//! grain=sRGB 符号値)と `recipe::validate` の委譲まで含めて検証するため。
//!
//! 期待値は `crates/atx-core/src/ops/finish.rs` に書かれた定義式
//! (smoothstep 減衰 / splitmix64 位置ハッシュ)を、このファイル内で
//! **独立に書き下した参照実装**から求めている(実装の出力を貼り付けたものではない)。

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe, AtxError, Limits};
use image::{ImageFormat, Rgba, RgbaImage};

const FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");

const PNG_ENCODE: &str = r#"{"op":"encode","format":"png"}"#;

fn recipe(json: &str) -> TransformRecipe {
    serde_json::from_str(json).expect("recipe should parse")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

fn decode(bytes: &[u8]) -> RgbaImage {
    image::load_from_memory(bytes).unwrap().to_rgba8()
}

/// PNG 経由(可逆)でレシピを適用し、結果の RGBA8 を返す。
fn run(img: &RgbaImage, json: &str) -> RgbaImage {
    decode(&run_bytes(img, json))
}

/// 適用結果の生バイト列(PNG 出力)。決定論・恒等の比較用。
fn run_bytes(img: &RgbaImage, json: &str) -> Vec<u8> {
    apply_recipe(&encode_png(img), &recipe(json), &Limits::default())
        .expect("recipe should apply")
        .bytes
}

fn err(img: &RgbaImage, json: &str) -> AtxError {
    apply_recipe(&encode_png(img), &recipe(json), &Limits::default())
        .expect_err("recipe should be rejected")
}

/// 検証エラーが `operations[index] (op_name):` を名指ししていること。
fn assert_rejected_at(e: &AtxError, index: usize, op_name: &str) {
    match e {
        AtxError::InvalidRecipe(msg) => assert!(
            msg.contains(&format!("operations[{index}] ({op_name})")),
            "error should name operations[{index}] ({op_name}), got: {msg}"
        ),
        other => panic!("expected InvalidRecipe, got {other:?}"),
    }
}

/// 全画素が異なる 4x3 のプローブ画像(反転の写像を1画素単位で追える)。
fn probe_image() -> RgbaImage {
    RgbaImage::from_fn(4, 3, |x, y| {
        Rgba([
            (10 + x * 40) as u8,
            (20 + y * 60) as u8,
            (x * 4 + y) as u8,
            255,
        ])
    })
}

/// 一様グレーの正方画像(ビネット・グレインの解析対象)。
fn gray_image(size: u32, value: u8) -> RgbaImage {
    RgbaImage::from_pixel(size, size, Rgba([value, value, value, 255]))
}

// ------------------------------------------------- 参照実装(期待値の独立計算)

/// sRGB EOTF(符号値 → 線形光)。
fn eotf(c: f64) -> f64 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB OETF(線形光 → 符号値)。
fn oetf(l: f64) -> f64 {
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// finish.rs の減衰曲線をテスト側で独立に書き下したもの。
fn expected_gain(d: f64, strength: f64, radius: f64, feather: f64) -> f64 {
    let g = if d <= radius {
        1.0
    } else if feather <= 0.0 || d >= radius + feather {
        1.0 - strength
    } else {
        let t = (d - radius) / feather;
        1.0 - strength * (t * t * (3.0 - 2.0 * t))
    };
    (g * 1e6).round() / 1e6
}

/// 画素 (x, y) の正規化中心距離(半対角比)。
fn norm_distance(x: u32, y: u32, w: u32, h: u32) -> f64 {
    let dx = (x as f64 + 0.5) - w as f64 / 2.0;
    let dy = (y as f64 + 0.5) - h as f64 / 2.0;
    let half_diag = ((w as f64).powi(2) + (h as f64).powi(2)).sqrt() / 2.0;
    (dx * dx + dy * dy).sqrt() / half_diag
}

/// 「u8 入力 → 線形 → ゲイン → sRGB → u8」の期待値。
fn expected_vignette_u8(v: u8, gain: f64) -> f64 {
    let linear = eotf(v as f64 / 255.0) * gain;
    oetf(linear.clamp(0.0, 1.0)) * 255.0
}

// ---------------------------------------------------------------------- flip

/// 水平反転を 2 回かけると **バイト同一**で元に戻る
/// (flip は画素値に触れない置換なので空間変換すら挟まない)。
#[test]
fn horizontal_flip_twice_is_byte_identical() {
    let src = probe_image();
    let json = format!(
        r#"{{"operations":[{{"op":"flip","direction":"horizontal"}},
            {{"op":"flip","direction":"horizontal"}},{PNG_ENCODE}]}}"#
    );
    let identity = format!(r#"{{"operations":[{PNG_ENCODE}]}}"#);
    assert_eq!(run_bytes(&src, &json), run_bytes(&src, &identity));
}

/// 垂直反転も 2 回でバイト同一。
#[test]
fn vertical_flip_twice_is_byte_identical() {
    let src = probe_image();
    let json = format!(
        r#"{{"operations":[{{"op":"flip","direction":"vertical"}},
            {{"op":"flip","direction":"vertical"}},{PNG_ENCODE}]}}"#
    );
    let identity = format!(r#"{{"operations":[{PNG_ENCODE}]}}"#);
    assert_eq!(run_bytes(&src, &json), run_bytes(&src, &identity));
}

/// 水平反転は `out(x, y) = in(w - 1 - x, y)`(全画素で厳密)。
#[test]
fn horizontal_flip_mirrors_x() {
    let src = probe_image();
    let json =
        format!(r#"{{"operations":[{{"op":"flip","direction":"horizontal"}},{PNG_ENCODE}]}}"#);
    let out = run(&src, &json);
    assert_eq!(out.dimensions(), src.dimensions());
    let (w, h) = src.dimensions();
    for y in 0..h {
        for x in 0..w {
            assert_eq!(
                out.get_pixel(x, y),
                src.get_pixel(w - 1 - x, y),
                "mismatch at ({x},{y})"
            );
        }
    }
    // 代表プローブ: 左上には元の右上が来る。
    assert_eq!(out.get_pixel(0, 0), src.get_pixel(3, 0));
}

/// 垂直反転は `out(x, y) = in(x, h - 1 - y)`。
#[test]
fn vertical_flip_mirrors_y() {
    let src = probe_image();
    let json = format!(r#"{{"operations":[{{"op":"flip","direction":"vertical"}},{PNG_ENCODE}]}}"#);
    let out = run(&src, &json);
    let (w, h) = src.dimensions();
    for y in 0..h {
        for x in 0..w {
            assert_eq!(
                out.get_pixel(x, y),
                src.get_pixel(x, h - 1 - y),
                "mismatch at ({x},{y})"
            );
        }
    }
    assert_eq!(out.get_pixel(0, 0), src.get_pixel(0, 2));
}

// ------------------------------------------------------------------ vignette

/// `strength = 0` は恒等 = **バイト同一**(線形往復もバイト同一なので出力が動かない)。
#[test]
fn vignette_zero_strength_is_byte_identical() {
    let src = gray_image(32, 128);
    let json = format!(
        r#"{{"operations":[{{"op":"vignette","strength":0.0,"radius":0.5,"feather":0.5}},
            {PNG_ENCODE}]}}"#
    );
    let identity = format!(r#"{{"operations":[{PNG_ENCODE}]}}"#);
    assert_eq!(run_bytes(&src, &json), run_bytes(&src, &identity));
}

/// 中心付近(d < radius)の画素はゲイン 1.0 なので値が変わらない。
#[test]
fn vignette_leaves_center_untouched() {
    let src = gray_image(64, 128);
    let json = format!(
        r#"{{"operations":[{{"op":"vignette","strength":0.5,"radius":0.5,"feather":0.5}},
            {PNG_ENCODE}]}}"#
    );
    let out = run(&src, &json);
    // d(32,32) ≈ 0.016 < radius。
    assert!(norm_distance(32, 32, 64, 64) < 0.5);
    assert_eq!(out.get_pixel(32, 32), &Rgba([128, 128, 128, 255]));
    assert_eq!(out.get_pixel(31, 31), &Rgba([128, 128, 128, 255]));
}

/// 角の画素が「同じ式 + 同じ符号化」で計算した期待値と ±1 で一致する。
#[test]
fn vignette_corner_matches_the_reference_formula() {
    let src = gray_image(64, 128);
    let json = format!(
        r#"{{"operations":[{{"op":"vignette","strength":0.5,"radius":0.5,"feather":0.5}},
            {PNG_ENCODE}]}}"#
    );
    let out = run(&src, &json);

    for (x, y) in [(0u32, 0u32), (63, 0), (0, 63), (63, 63)] {
        let d = norm_distance(x, y, 64, 64);
        let gain = expected_gain(d, 0.5, 0.5, 0.5);
        let expected = expected_vignette_u8(128, gain);
        let got = out.get_pixel(x, y).0[0] as f64;
        assert!(
            (got - expected).abs() <= 1.0,
            "corner ({x},{y}): d={d}, gain={gain}, expected≈{expected}, got {got}"
        );
        // 実際に暗くなっていること(ゲイン ~0.5 は 8bit で大きな差になる)。
        assert!(
            got < 128.0,
            "corner ({x},{y}) should be darkened, got {got}"
        );
    }
    // アルファは不変。
    assert_eq!(out.get_pixel(0, 0).0[3], 255);
}

/// 負の strength は角を**明るく**する(ゲイン > 1)。
#[test]
fn vignette_negative_strength_brightens_the_corner() {
    let src = gray_image(64, 128);
    let json = format!(
        r#"{{"operations":[{{"op":"vignette","strength":-0.5,"radius":0.5,"feather":0.5}},
            {PNG_ENCODE}]}}"#
    );
    let out = run(&src, &json);
    let d = norm_distance(0, 0, 64, 64);
    let gain = expected_gain(d, -0.5, 0.5, 0.5);
    assert!(gain > 1.0, "gain should exceed 1.0, got {gain}");
    let expected = expected_vignette_u8(128, gain);
    let got = out.get_pixel(0, 0).0[0] as f64;
    assert!(
        (got - expected).abs() <= 1.0,
        "expected≈{expected}, got {got}"
    );
    assert!(got > 128.0, "corner should be brightened, got {got}");
    // 中心は変わらない。
    assert_eq!(out.get_pixel(32, 32).0[0], 128);
}

// --------------------------------------------------------------------- grain

/// `amount = 0` は恒等 = バイト同一。
#[test]
fn grain_zero_amount_is_byte_identical() {
    let src = gray_image(32, 128);
    let json = format!(
        r#"{{"operations":[{{"op":"grain","amount":0.0,"size":1,"monochrome":true,"seed":7}},
            {PNG_ENCODE}]}}"#
    );
    let identity = format!(r#"{{"operations":[{PNG_ENCODE}]}}"#);
    assert_eq!(run_bytes(&src, &json), run_bytes(&src, &identity));
}

/// 同じ seed で 2 回走らせるとバイト同一(乱数状態を持たない位置ハッシュ)。
#[test]
fn grain_is_deterministic_for_the_same_seed() {
    let src = gray_image(32, 128);
    let json = format!(
        r#"{{"operations":[{{"op":"grain","amount":0.5,"size":1,"monochrome":true,"seed":42}},
            {PNG_ENCODE}]}}"#
    );
    assert_eq!(run_bytes(&src, &json), run_bytes(&src, &json));
}

/// seed が違えば出力も違う。
#[test]
fn grain_different_seed_gives_different_bytes() {
    let src = gray_image(32, 128);
    let a = format!(
        r#"{{"operations":[{{"op":"grain","amount":0.5,"size":1,"monochrome":true,"seed":1}},
            {PNG_ENCODE}]}}"#
    );
    let b = format!(
        r#"{{"operations":[{{"op":"grain","amount":0.5,"size":1,"monochrome":true,"seed":2}},
            {PNG_ENCODE}]}}"#
    );
    assert_ne!(run_bytes(&src, &a), run_bytes(&src, &b));
}

/// `monochrome = true` はグレー画像上で R == G == B を保つ(輝度だけが揺れる)。
/// `false` はチャンネル独立なので、どこかの画素で必ず崩れる。
#[test]
fn grain_monochrome_keeps_channels_equal() {
    let src = gray_image(32, 128);
    let mono = format!(
        r#"{{"operations":[{{"op":"grain","amount":0.5,"size":1,"monochrome":true,"seed":9}},
            {PNG_ENCODE}]}}"#
    );
    let out = run(&src, &mono);
    let mut moved = 0usize;
    for p in out.pixels() {
        assert_eq!(p.0[0], p.0[1], "R/G must match under monochrome grain");
        assert_eq!(p.0[1], p.0[2], "G/B must match under monochrome grain");
        assert_eq!(p.0[3], 255, "alpha must be untouched");
        if p.0[0] != 128 {
            moved += 1;
        }
    }
    assert!(moved > 0, "grain should actually change pixels");

    let color = format!(
        r#"{{"operations":[{{"op":"grain","amount":0.5,"size":1,"monochrome":false,"seed":9}},
            {PNG_ENCODE}]}}"#
    );
    let out = run(&src, &color);
    assert!(
        out.pixels().any(|p| p.0[0] != p.0[1] || p.0[1] != p.0[2]),
        "color grain must decorrelate channels"
    );
}

/// ノイズは平均 0 なので、一様グレーの平均輝度は ±1 段以内に留まる。
#[test]
fn grain_preserves_the_mean_level() {
    let src = gray_image(128, 128);
    let json = format!(
        r#"{{"operations":[{{"op":"grain","amount":0.5,"size":1,"monochrome":true,"seed":3}},
            {PNG_ENCODE}]}}"#
    );
    let out = run(&src, &json);
    let sum: u64 = out.pixels().map(|p| u64::from(p.0[0])).sum();
    let mean = sum as f64 / out.pixels().len() as f64;
    assert!(
        (mean - 128.0).abs() <= 1.0,
        "mean level drifted: {mean} (expected ≈128)"
    );
    // 実際にばらついていること(平均が保たれるだけの恒等ではない)。
    let spread = out.pixels().filter(|p| p.0[0] != 128).count();
    assert!(spread > out.pixels().len() / 2, "grain barely moved pixels");
}

/// `size = 2` は 2x2 のブロック単位で一定のノイズを乗せる(最近傍のブロック拡大)。
#[test]
fn grain_size_two_produces_constant_2x2_blocks() {
    let src = gray_image(32, 128);
    let json = format!(
        r#"{{"operations":[{{"op":"grain","amount":0.6,"size":2,"monochrome":true,"seed":5}},
            {PNG_ENCODE}]}}"#
    );
    let out = run(&src, &json);
    let mut distinct_blocks = 0usize;
    for by in 0..16u32 {
        for bx in 0..16u32 {
            let v = out.get_pixel(bx * 2, by * 2).0[0];
            for dy in 0..2 {
                for dx in 0..2 {
                    assert_eq!(
                        out.get_pixel(bx * 2 + dx, by * 2 + dy).0[0],
                        v,
                        "block ({bx},{by}) is not constant"
                    );
                }
            }
            if v != 128 {
                distinct_blocks += 1;
            }
        }
    }
    assert!(distinct_blocks > 0, "size=2 grain should still be visible");
    // 隣接ブロックは無相関なので、少なくとも 1 組は値が違う。
    assert!(
        (0..16).any(|bx: u32| out.get_pixel(bx * 2, 0).0[0] != out.get_pixel(0, 0).0[0]),
        "adjacent blocks should differ"
    );
}

// ------------------------------------------------------------------ validate

/// 範囲外は op 添字つきで弾かれる(先頭に flip を置いて添字 1 を確かめる)。
#[test]
fn vignette_rejects_out_of_range_strength() {
    let src = gray_image(4, 128);
    for bad in ["1.5", "-1.5"] {
        let json = format!(
            r#"{{"operations":[{{"op":"flip","direction":"horizontal"}},
                {{"op":"vignette","strength":{bad}}},{PNG_ENCODE}]}}"#
        );
        let e = err(&src, &json);
        assert_rejected_at(&e, 1, "vignette");
        match &e {
            AtxError::InvalidRecipe(m) => assert!(m.contains("strength"), "message: {m}"),
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// 非有限値は JSON リテラルでは書けないので、レシピを直接組み立てて検証する。
#[test]
fn vignette_rejects_non_finite_parameters() {
    use atx_core::recipe::{Operation, TransformRecipe};
    let r = TransformRecipe {
        operations: vec![Operation::Vignette {
            strength: f64::NAN,
            radius: 0.5,
            feather: 0.5,
        }],
        layers: None,
    };
    let e = apply_recipe(&encode_png(&gray_image(4, 128)), &r, &Limits::default())
        .expect_err("NaN strength must be rejected");
    assert_rejected_at(&e, 0, "vignette");

    let r = TransformRecipe {
        operations: vec![Operation::Grain {
            amount: f64::INFINITY,
            size: 1,
            monochrome: true,
            seed: 0,
            mask: None,
        }],
        layers: None,
    };
    let e = apply_recipe(&encode_png(&gray_image(4, 128)), &r, &Limits::default())
        .expect_err("infinite amount must be rejected");
    assert_rejected_at(&e, 0, "grain");
}

#[test]
fn vignette_rejects_out_of_range_radius() {
    let src = gray_image(4, 128);
    for bad in ["-0.1", "1.6"] {
        let json = format!(
            r#"{{"operations":[{{"op":"vignette","strength":0.5,"radius":{bad}}},{PNG_ENCODE}]}}"#
        );
        let e = err(&src, &json);
        assert_rejected_at(&e, 0, "vignette");
        match &e {
            AtxError::InvalidRecipe(m) => assert!(m.contains("radius"), "message: {m}"),
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn vignette_rejects_out_of_range_feather() {
    let src = gray_image(4, 128);
    for bad in ["-0.1", "1.1"] {
        let json = format!(
            r#"{{"operations":[{{"op":"vignette","strength":0.5,"feather":{bad}}},{PNG_ENCODE}]}}"#
        );
        let e = err(&src, &json);
        assert_rejected_at(&e, 0, "vignette");
        match &e {
            AtxError::InvalidRecipe(m) => assert!(m.contains("feather"), "message: {m}"),
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn grain_rejects_out_of_range_amount() {
    let src = gray_image(4, 128);
    for bad in ["-0.1", "1.1"] {
        let json = format!(
            r#"{{"operations":[{{"op":"flip","direction":"vertical"}},
                {{"op":"grain","amount":{bad}}},{PNG_ENCODE}]}}"#
        );
        let e = err(&src, &json);
        assert_rejected_at(&e, 1, "grain");
        match &e {
            AtxError::InvalidRecipe(m) => assert!(m.contains("amount"), "message: {m}"),
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn grain_rejects_out_of_range_size() {
    let src = gray_image(4, 128);
    for bad in ["0", "5"] {
        let json = format!(
            r#"{{"operations":[{{"op":"grain","amount":0.5,"size":{bad}}},{PNG_ENCODE}]}}"#
        );
        let e = err(&src, &json);
        assert_rejected_at(&e, 0, "grain");
        match &e {
            AtxError::InvalidRecipe(m) => assert!(m.contains("size"), "message: {m}"),
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// 恒等指定(strength = 0 / amount = 0)は「無意味だが合法」— 検証を通す。
#[test]
fn identity_parameters_are_accepted() {
    let src = gray_image(4, 128);
    let json = format!(
        r#"{{"operations":[{{"op":"vignette","strength":0.0}},
            {{"op":"grain","amount":0.0}},{PNG_ENCODE}]}}"#
    );
    let _ = run_bytes(&src, &json);
}

// ------------------------------------------------------------------ ゴールデン

/// フルパイプラインのゴールデン(fixture + vignette + grain + JPEG エンコード)。
///
/// 合成フィクスチャ `tests/fixtures/synthetic_scene.jpg`
/// (`cargo run -p atx-core --example gen_fixture` で再生成可能)に対して
/// 出力 sha256 をピン留めする。engine.rs のゴールデンと同じ規律:
/// **意図的にアルゴリズムを変えた場合のみ `ENGINE_VERSION` を上げた上で更新すること**。
/// フィクスチャを作り直した場合もこの値の更新が必要。
///
/// vignette(線形光)→ grain(sRGB 符号値)の順なので、空間変換が 1 回だけ
/// 挟まる経路も同時に固定している。
#[test]
fn golden_vignette_grain_pipeline_sha256() {
    let r = recipe(
        r#"{"operations":[
            {"op":"vignette","strength":0.6,"radius":0.45,"feather":0.55},
            {"op":"grain","amount":0.35,"size":2,"monochrome":true,"seed":20250821},
            {"op":"encode","format":"jpeg","quality":85}
        ]}"#,
    );
    let out = apply_recipe(FIXTURE, &r, &Limits::default()).unwrap();
    assert_eq!(atx_core::ENGINE_VERSION, "atx-core/2");
    assert_eq!(
        sha256_hex(&out.bytes),
        // v0.3.0 で新規に固定した値(vignette = smoothstep 減衰、grain = splitmix64 位置ハッシュ)。
        "33b164b97c2231121641ee347d0d7d5a486af89c3aafc2e714b81e5a3db19a04",
        "finish op golden moved"
    );
}
