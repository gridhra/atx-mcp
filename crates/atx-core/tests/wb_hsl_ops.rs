//! `white_balance` / `hsl`(v0.3 の色補正系 op)のテスト。
//!
//! すべて `apply_recipe`(JSON レシピ)経由で実行する。ユニット呼び出しではなく
//! エンジンのディスパッチと `recipe::validate` の委譲まで含めて検証するため
//! (`ops` モジュールは `pub(crate)` なので統合テストからは直接触れない。
//! 変換関数単体の細かい検証は `src/ops/hsl.rs` の `mod tests` 側にある)。
//!
//! 期待値は `src/ops/wb.rs` / `src/ops/hsl.rs` のモジュールドキュメントに書かれた
//! 定義式から手計算したもの。実装の出力を貼り付けたものではない。

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

/// 適用結果の生バイト列(PNG 出力)。決定論・バイト一致の比較用。
fn run_bytes(img: &RgbaImage, json: &str) -> Vec<u8> {
    apply_recipe(&encode_png(img), &recipe(json), &Limits::default())
        .expect("recipe should apply")
        .bytes
}

fn err(img: &RgbaImage, json: &str) -> String {
    match apply_recipe(&encode_png(img), &recipe(json), &Limits::default()) {
        Err(AtxError::InvalidRecipe(m)) => m,
        other => panic!("expected InvalidRecipe, got {other:?}"),
    }
}

fn pixels(img: &RgbaImage) -> Vec<[u8; 4]> {
    img.pixels().map(|p| p.0).collect()
}

/// 代表色 + 中間グレー(半透明)を並べた 6x1 のプローブ画像。
/// index: 0=red 1=green 2=blue 3=grey128(alpha 200) 4=orange寄り 5=teal寄り
fn probe_image() -> RgbaImage {
    let px = [
        Rgba([255, 0, 0, 255]),
        Rgba([0, 255, 0, 255]),
        Rgba([0, 0, 255, 255]),
        Rgba([128, 128, 128, 200]),
        Rgba([220, 140, 40, 255]),
        Rgba([20, 160, 170, 255]),
    ];
    RgbaImage::from_fn(px.len() as u32, 1, |x, _| px[x as usize])
}

/// 無彩色のみのランプ(WB のチャンネルゲインを直接読み出せる)。
fn grey_ramp() -> RgbaImage {
    RgbaImage::from_fn(256, 1, |x, _| {
        let v = x as u8;
        Rgba([v, v, v, 255])
    })
}

/// BT.709 の平均輝度。
fn mean_luma(img: &RgbaImage) -> f64 {
    let mut sum = 0.0;
    for p in img.pixels() {
        sum += 0.2126 * p.0[0] as f64 + 0.7152 * p.0[1] as f64 + 0.0722 * p.0[2] as f64;
    }
    sum / img.pixels().len() as f64
}

// ==================================================================== white_balance

/// temperature=0 / tint=0 は 1 バイトも変えない(短絡経路)。
#[test]
fn wb_identity_is_byte_identical() {
    let src = probe_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[{{"op":"white_balance","temperature":0,"tint":0}},{PNG_ENCODE}]}}"#
        ),
    );
    assert_eq!(pixels(&out), pixels(&src));

    // op を丸ごと省略した場合(serde default = 0.0)も同じ。
    let out2 = run(
        &src,
        &format!(r#"{{"operations":[{{"op":"white_balance"}},{PNG_ENCODE}]}}"#),
    );
    assert_eq!(pixels(&out2), pixels(&src));
}

/// 暖色方向(temperature > 0)は R を上げ B を下げる。無彩色ランプで読む。
///
/// 期待値はモデルから手計算: t=100 →
/// g_r=1.35, g_g=1.0, g_b=0.65 / mean = 0.2126*1.35 + 0.7152*1.0 + 0.0722*0.65
/// = 0.28701 + 0.7152 + 0.04693 = 1.04914 →
/// G_r ≈ 1.286671, G_g ≈ 0.953165, G_b ≈ 0.619554。
#[test]
fn wb_warm_raises_red_and_lowers_blue() {
    let out = run(
        &grey_ramp(),
        &format!(
            r#"{{"operations":[{{"op":"white_balance","temperature":100,"tint":0}},{PNG_ENCODE}]}}"#
        ),
    );
    // 128 の画素で手計算値と一致すること。
    let px = out.get_pixel(128, 0).0;
    assert_eq!(px[0], (128.0f64 * 1.286671).round() as u8);
    assert_eq!(px[1], (128.0f64 * 0.953165).round() as u8);
    assert_eq!(px[2], (128.0f64 * 0.619554).round() as u8);
    assert!(px[0] > 128 && px[2] < 128, "warm: R up, B down, got {px:?}");
    assert_eq!(px[3], 255, "alpha untouched");

    // 寒色方向は逆(単調性)。
    let cool = run(
        &grey_ramp(),
        &format!(
            r#"{{"operations":[{{"op":"white_balance","temperature":-100,"tint":0}},{PNG_ENCODE}]}}"#
        ),
    );
    let cp = cool.get_pixel(128, 0).0;
    assert!(cp[0] < 128 && cp[2] > 128, "cool: R down, B up, got {cp:?}");
}

/// tint 正(マゼンタ)は G を下げ、tint 負(グリーン)は G を上げる。
#[test]
fn wb_magenta_tint_lowers_green() {
    let magenta = run(
        &grey_ramp(),
        &format!(
            r#"{{"operations":[{{"op":"white_balance","temperature":0,"tint":100}},{PNG_ENCODE}]}}"#
        ),
    );
    let m = magenta.get_pixel(160, 0).0;
    assert!(m[1] < 160, "magenta tint must lower green, got {m:?}");
    assert!(
        m[0] > 160 && m[2] > 160,
        "輝度正規化で R/B は持ち上がる, got {m:?}"
    );
    assert_eq!(m[0], m[2], "tint は R/B を対称に扱う");

    let green = run(
        &grey_ramp(),
        &format!(
            r#"{{"operations":[{{"op":"white_balance","temperature":0,"tint":-100}},{PNG_ENCODE}]}}"#
        ),
    );
    let g = green.get_pixel(160, 0).0;
    assert!(g[1] > 160, "green tint must raise green, got {g:?}");
}

/// 輝度加重平均によるゲイン正規化で、フィクスチャの平均輝度はおおむね保たれる(2% 以内)。
///
/// 厳密な保存ではない(画素ごとの 0..255 クランプがあるため)。極端な設定でも
/// 2% に収まることをここで固定しておく。
#[test]
fn wb_preserves_mean_luma_within_two_percent() {
    let base = decode(FIXTURE);
    let before = mean_luma(&base);
    for (t, m) in [
        (100.0, 0.0),
        (-100.0, 0.0),
        (0.0, 100.0),
        (0.0, -100.0),
        (75.0, -50.0),
    ] {
        let out = run(
            &base,
            &format!(
                r#"{{"operations":[{{"op":"white_balance","temperature":{t},"tint":{m}}},{PNG_ENCODE}]}}"#
            ),
        );
        let after = mean_luma(&out);
        let ratio = (after - before).abs() / before;
        assert!(
            ratio < 0.02,
            "mean luma drifted {:.3}% for (t={t}, tint={m}): {before:.2} -> {after:.2}",
            ratio * 100.0
        );
    }
}

/// アルファは WB の対象外。
#[test]
fn wb_leaves_alpha_untouched() {
    let src = probe_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[{{"op":"white_balance","temperature":80,"tint":-30}},{PNG_ENCODE}]}}"#
        ),
    );
    for (a, b) in src.pixels().zip(out.pixels()) {
        assert_eq!(a.0[3], b.0[3]);
    }
}

/// 同じレシピは同じバイト列を返す。
#[test]
fn wb_is_deterministic() {
    let src = probe_image();
    let json = format!(
        r#"{{"operations":[{{"op":"white_balance","temperature":37.5,"tint":-12.25}},{PNG_ENCODE}]}}"#
    );
    assert_eq!(run_bytes(&src, &json), run_bytes(&src, &json));
}

/// 範囲外・非有限値は validate で弾く。
#[test]
fn wb_rejects_out_of_range_sliders() {
    let src = probe_image();
    for (json, needle) in [
        (
            r#"{"operations":[{"op":"white_balance","temperature":100.5}]}"#,
            "temperature",
        ),
        (
            r#"{"operations":[{"op":"white_balance","tint":-101}]}"#,
            "tint",
        ),
        (
            r#"{"operations":[{"op":"white_balance","temperature":-1000,"tint":0}]}"#,
            "temperature",
        ),
    ] {
        let m = err(&src, json);
        assert!(
            m.contains("white_balance") && m.contains(needle),
            "unexpected message: {m}"
        );
    }
}

// ============================================================================ hsl

/// 全帯の指定が 0(または未指定)なら 1 バイトも変わらない。
#[test]
fn hsl_all_zero_shifts_is_byte_identical() {
    let src = probe_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[
                {{"op":"hsl","red":{{"hue":0,"saturation":0,"luminance":0}},"blue":{{}}}},
                {PNG_ENCODE}]}}"#
        ),
    );
    assert_eq!(pixels(&out), pixels(&src));
}

/// **中核の品質ゲート**: RGB → HSL → RGB の往復がバイト一致であること。
///
/// r/g/b を 5 刻み(52 値)で総当たりした 52^3 = 140,608 画素の画像を作り、
/// 全 8 帯に「実質ゼロだが短絡はされない」極小の saturation(1e-6)を指定して
/// **全有彩色画素を変換往復に通す**。saturation 1e-6 の乗算係数は 1e-8 なので
/// 丸め後の u8 は動かないはずで、出力は入力とバイト一致でなければならない。
///
/// (5 刻みの理由は実行時間。`src/ops/hsl.rs` の unit test 側にも粗い格子の
/// 往復テストがあり、全 16,777,216 値での網羅は開発時にローカル確認済み。)
#[test]
fn hsl_rgb_roundtrip_is_byte_identical_on_dense_grid() {
    let values: Vec<u8> = (0..=51u32).map(|i| (i * 5) as u8).collect();
    let n = values.len();
    let src = RgbaImage::from_fn((n * n) as u32, n as u32, |x, y| {
        let r = values[(x as usize) / n];
        let g = values[(x as usize) % n];
        let b = values[y as usize];
        Rgba([r, g, b, 255])
    });
    assert_eq!(src.pixels().len(), 140_608);

    let band = r#"{"hue":0,"saturation":0.000001,"luminance":0}"#;
    let json = format!(
        r#"{{"operations":[{{"op":"hsl",
            "red":{band},"orange":{band},"yellow":{band},"green":{band},
            "aqua":{band},"blue":{band},"purple":{band},"magenta":{band}}},
            {PNG_ENCODE}]}}"#
    );
    let out = run(&src, &json);
    let mismatches: Vec<_> = src
        .pixels()
        .zip(out.pixels())
        .filter(|(a, b)| a.0 != b.0)
        .take(5)
        .map(|(a, b)| (a.0, b.0))
        .collect();
    assert!(
        mismatches.is_empty(),
        "RGB->HSL->RGB must be exact, first mismatches: {mismatches:?}"
    );
}

/// 帯の分離: red 帯の hue シフトは純赤を橙へ動かし、純青には触れない。
///
/// 手計算: 純赤 (255,0,0) は h=0(red 中心、重み 1)、s=1、l=0.5。
/// hue=+100 → dh = 100 * 0.3 = 30° → h'=30。c=1, hp=0.5, x=0.5, m=0 なので
/// (1, 0.5, 0) → (255, 128, 0)(0.5*255=127.5 を half-away-from-zero で 128)。
/// 純青 (0,0,255) は h=240(blue 中心、重み 1、隣は purple)。どちらもゼロ指定なので
/// 画素ごとの短絡で完全に不変。
#[test]
fn hsl_red_hue_shift_moves_red_to_orange_and_leaves_blue_alone() {
    let src = RgbaImage::from_fn(3, 1, |x, _| match x {
        0 => Rgba([255, 0, 0, 255]),
        1 => Rgba([0, 0, 255, 255]),
        _ => Rgba([90, 90, 90, 255]),
    });
    let out = run(
        &src,
        &format!(r#"{{"operations":[{{"op":"hsl","red":{{"hue":100}}}},{PNG_ENCODE}]}}"#),
    );
    assert_eq!(out.get_pixel(0, 0).0, [255, 128, 0, 255], "red -> orange");
    assert_eq!(out.get_pixel(1, 0).0, [0, 0, 255, 255], "blue untouched");
    assert_eq!(out.get_pixel(2, 0).0, [90, 90, 90, 255], "grey untouched");

    // 逆向き(hue=-100)はマゼンタ方向へ: h' = -30 → 330°。
    let back = run(
        &src,
        &format!(r#"{{"operations":[{{"op":"hsl","red":{{"hue":-100}}}},{PNG_ENCODE}]}}"#),
    );
    assert_eq!(back.get_pixel(0, 0).0, [255, 0, 128, 255], "red -> magenta");
}

/// 三角フェザ: 帯中心の中間(red 0° と orange 30° の中点 15°)では重みが 0.5 ずつ。
///
/// h=15 の画素に red だけ hue=+100 を掛けると dh = 0.5 * 100 * 0.3 = 15° で h'=30。
#[test]
fn hsl_feather_halves_the_shift_between_two_band_centers() {
    // h=15, s=1, l=0.5 → c=1, hp=0.25, x=0.25, m=0 → (255, 64, 0)(63.75 → 64)。
    let src = RgbaImage::from_fn(1, 1, |_, _| Rgba([255, 64, 0, 255]));
    assert_eq!(
        run(
            &src,
            &format!(r#"{{"operations":[{{"op":"hsl","red":{{"hue":100}}}},{PNG_ENCODE}]}}"#)
        )
        .get_pixel(0, 0)
        .0,
        // h' = 30 → (255, 128, 0)
        [255, 128, 0, 255]
    );
}

/// saturation = -100 は乗算係数のクランプ下限 -0.95 に当たり、彩度は元の 5% になる。
///
/// 完全なグレースケール化ではない(単調性を残すため 0 倍は許さない設計)。
/// 純赤 (255,0,0): s=1 → 0.05, l=0.5 → c=0.05, x=0, m=0.475 →
/// (0.525, 0.475, 0.475) * 255 = (133.875, 121.125, 121.125) → (134, 121, 121)。
/// チャンネル差は 255 → 13 まで縮む。
#[test]
fn hsl_saturation_minus_100_leaves_only_five_percent_of_chroma() {
    let src = probe_image();
    let band = r#"{"saturation":-100}"#;
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[{{"op":"hsl",
                "red":{band},"orange":{band},"yellow":{band},"green":{band},
                "aqua":{band},"blue":{band},"purple":{band},"magenta":{band}}},
                {PNG_ENCODE}]}}"#
        ),
    );
    assert_eq!(out.get_pixel(0, 0).0, [134, 121, 121, 255], "pure red");
    for (before, after) in src.pixels().zip(out.pixels()) {
        let spread =
            |p: &[u8; 4]| p[0].max(p[1]).max(p[2]) as i32 - p[0].min(p[1]).min(p[2]) as i32;
        let (b0, a0) = (spread(&before.0), spread(&after.0));
        assert!(
            a0 <= (b0 / 10).max(1) + 2,
            "chroma spread must collapse: {b0} -> {a0} ({:?} -> {:?})",
            before.0,
            after.0
        );
        assert_eq!(before.0[3], after.0[3], "alpha untouched");
    }
}

/// luminance シフトは明度を乗算的に動かす(±100 で ±50%)。
#[test]
fn hsl_luminance_shift_scales_lightness() {
    // 青 (0,0,255): h=240, s=1, l=0.5。blue 帯に luminance=+100 → f=+0.5 → l'=0.75。
    // c = (2-1.5)*1 = 0.5, hp=4, x=0, m=0.5 → (0.5, 0.5, 1.0) → (128,128,255)。
    let src = RgbaImage::from_fn(1, 1, |_, _| Rgba([0, 0, 255, 255]));
    let up = run(
        &src,
        &format!(r#"{{"operations":[{{"op":"hsl","blue":{{"luminance":100}}}},{PNG_ENCODE}]}}"#),
    );
    assert_eq!(up.get_pixel(0, 0).0, [128, 128, 255, 255]);

    // luminance=-100 → f = -0.5 → l'=0.25 → c=0.5, m=0 → (0,0,0.5) → (0,0,128)。
    let down = run(
        &src,
        &format!(r#"{{"operations":[{{"op":"hsl","blue":{{"luminance":-100}}}},{PNG_ENCODE}]}}"#),
    );
    assert_eq!(down.get_pixel(0, 0).0, [0, 0, 128, 255]);
}

/// 同じレシピは同じバイト列を返す。
#[test]
fn hsl_is_deterministic() {
    let src = probe_image();
    let json = format!(
        r#"{{"operations":[{{"op":"hsl",
            "orange":{{"hue":-33.5,"saturation":42.25,"luminance":-7.5}},
            "aqua":{{"saturation":-60}}}},
            {PNG_ENCODE}]}}"#
    );
    assert_eq!(run_bytes(&src, &json), run_bytes(&src, &json));
}

/// 帯が 1 つも指定されていない hsl はエラー。
#[test]
fn hsl_rejects_empty_band_set() {
    let m = err(&probe_image(), r#"{"operations":[{"op":"hsl"}]}"#);
    assert!(
        m.contains("hsl") && m.contains("at least one"),
        "unexpected message: {m}"
    );
}

/// 範囲外のスライダはエラー(帯名とフィールド名がメッセージに出る)。
#[test]
fn hsl_rejects_out_of_range_sliders() {
    for (json, needle) in [
        (
            r#"{"operations":[{"op":"hsl","green":{"hue":120}}]}"#,
            "green.hue",
        ),
        (
            r#"{"operations":[{"op":"hsl","purple":{"saturation":-100.5}}]}"#,
            "purple.saturation",
        ),
        (
            r#"{"operations":[{"op":"hsl","magenta":{"luminance":1000}}]}"#,
            "magenta.luminance",
        ),
    ] {
        let m = err(&probe_image(), json);
        assert!(
            m.contains("hsl") && m.contains(needle),
            "unexpected message: {m}"
        );
    }
}

// ====================================================================== ゴールデン

/// フルパイプラインのゴールデン(white_balance + JPEG エンコード)。
///
/// 合成フィクスチャ `tests/fixtures/synthetic_scene.jpg`
/// (`cargo run -p atx-core --example gen_fixture` で再生成可能)に対して
/// 出力 sha256 をピン留めする。engine.rs のゴールデンと同じ規律:
/// **意図的にゲインモデルを変えた場合のみ `ENGINE_VERSION` を上げた上で更新すること**。
/// フィクスチャを作り直した場合もこの値の更新が必要。
#[test]
fn golden_white_balance_pipeline_sha256() {
    let r = recipe(
        r#"{"operations":[
            {"op":"white_balance","temperature":45,"tint":-20},
            {"op":"encode","format":"jpeg","quality":85}
        ]}"#,
    );
    let out = apply_recipe(FIXTURE, &r, &Limits::default()).unwrap();
    assert_eq!(atx_core::ENGINE_VERSION, "atx-core/1");
    assert_eq!(
        sha256_hex(&out.bytes),
        "bc5348305c094479e20f2397eefc0a863754dff19532393cc8d0ce97d3673604",
        "white_balance golden"
    );
}

/// フルパイプラインのゴールデン(hsl + JPEG エンコード)。規律は上と同じ。
#[test]
fn golden_hsl_pipeline_sha256() {
    let r = recipe(
        r#"{"operations":[
            {"op":"hsl",
             "red":{"hue":-15,"saturation":25,"luminance":-10},
             "yellow":{"saturation":-40},
             "blue":{"hue":8,"luminance":20}},
            {"op":"encode","format":"jpeg","quality":85}
        ]}"#,
    );
    let out = apply_recipe(FIXTURE, &r, &Limits::default()).unwrap();
    assert_eq!(atx_core::ENGINE_VERSION, "atx-core/1");
    assert_eq!(
        sha256_hex(&out.bytes),
        "054f3dbc85e629067dd26e635c723b9e7d222d2faad9d4c008ccbd52a2757e4f",
        "hsl golden"
    );
}
