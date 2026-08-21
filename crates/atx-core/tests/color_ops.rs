//! color_matrix / curves / levels(v0.2 トーン・カラー系メタ op)のテスト。
//!
//! すべて `apply_recipe`(JSON レシピ)経由で実行する。ユニット呼び出しではなく
//! エンジンのディスパッチと `recipe::validate` の委譲まで含めて検証するため。
//!
//! 期待値は `crates/atx-core/src/ops/color.rs` に書かれた定義式
//! (0..1 正規化 → 行列 → クランプ → half-away-from-zero 丸め / 256 LUT)から
//! 手計算したもの。実装の出力をそのまま貼り付けたものではない。

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe, AtxError, Limits};
use image::{ImageFormat, Rgba, RgbaImage};

const FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");

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

/// 代表色 + 半透明グレーを並べた 5x1 のプローブ画像。
/// アルファ 200 の画素で「curves/levels はアルファに触れない / color_matrix は触れる」
/// を同時に検証できるようにしてある。
fn probe_image() -> RgbaImage {
    let px = [
        Rgba([255, 0, 0, 255]),
        Rgba([0, 255, 0, 255]),
        Rgba([0, 0, 255, 255]),
        Rgba([128, 128, 128, 200]),
        Rgba([10, 200, 90, 255]),
    ];
    RgbaImage::from_fn(px.len() as u32, 1, |x, _| px[x as usize])
}

/// 0..255 の水平グラデーション(R=G=B=x、不透明)。LUT の形をそのまま読み出せる。
fn ramp_image() -> RgbaImage {
    RgbaImage::from_fn(256, 1, |x, _| {
        let v = x as u8;
        Rgba([v, v, v, 255])
    })
}

/// PNG 経由(可逆)でレシピを適用し、結果の RGBA8 を返す。
fn run(img: &RgbaImage, json: &str) -> RgbaImage {
    let bytes = apply_recipe(&encode_png(img), &recipe(json), &Limits::default())
        .expect("recipe should apply")
        .bytes;
    decode(&bytes)
}

/// 適用結果の生バイト列(PNG 出力)。決定論の比較用。
fn run_bytes(img: &RgbaImage, json: &str) -> Vec<u8> {
    apply_recipe(&encode_png(img), &recipe(json), &Limits::default())
        .expect("recipe should apply")
        .bytes
}

fn pixels(img: &RgbaImage) -> Vec<[u8; 4]> {
    img.pixels().map(|p| p.0).collect()
}

const PNG_ENCODE: &str = r#"{"op":"encode","format":"png"}"#;

// ------------------------------------------------------------- color_matrix

/// 恒等 4×5 行列は画像を1バイトも変えない(アルファ行も恒等)。
#[test]
fn color_matrix_identity_is_byte_identical() {
    let src = probe_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[
                {{"op":"color_matrix","matrix":[
                    1,0,0,0,0,
                    0,1,0,0,0,
                    0,0,1,0,0,
                    0,0,0,1,0]}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    assert_eq!(
        out.as_raw(),
        src.as_raw(),
        "identity matrix must be a no-op"
    );
}

/// BT.709 輝度行列は R==G==B のグレースケールを作り、アルファは恒等行で保存される。
#[test]
fn color_matrix_bt709_grayscale() {
    let src = probe_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[
                {{"op":"color_matrix","matrix":[
                    0.2126,0.7152,0.0722,0,0,
                    0.2126,0.7152,0.0722,0,0,
                    0.2126,0.7152,0.0722,0,0,
                    0,0,0,1,0]}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    for (i, p) in pixels(&out).iter().enumerate() {
        assert_eq!(p[0], p[1], "pixel {i}: R must equal G");
        assert_eq!(p[1], p[2], "pixel {i}: G must equal B");
    }
    // 手計算値: round(clamp(0.2126R+0.7152G+0.0722B) * 255)。
    assert_eq!(
        pixels(&out),
        vec![
            [54, 54, 54, 255],
            [182, 182, 182, 255],
            [18, 18, 18, 255],
            [128, 128, 128, 200],
            [152, 152, 152, 255],
        ]
    );
}

/// セピア行列のゴールデン(小さな合成画像の全画素を厳密比較)。
#[test]
fn color_matrix_sepia_golden_pixels() {
    let src = probe_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[
                {{"op":"color_matrix","matrix":[
                    0.393,0.769,0.189,0,0,
                    0.349,0.686,0.168,0,0,
                    0.272,0.534,0.131,0,0,
                    0,0,0,1,0]}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    // 定義式から手計算。例: 赤(255,0,0) → R'=0.393 → 0.393*255=100.215 → 100、
    // G'=0.349*255=88.995 → 89、B'=0.272*255=69.36 → 69。
    assert_eq!(
        pixels(&out),
        vec![
            [100, 89, 69, 255],
            [196, 175, 136, 255],
            [48, 43, 33, 255],
            [173, 154, 120, 200],
            [175, 156, 121, 255],
        ]
    );
}

/// color_matrix はアルファ行を持つので **アルファも変換対象**。
/// 4行目を [0,0,0,0.5,0] にするとアルファが半分になる。
#[test]
fn color_matrix_transforms_alpha() {
    let src = probe_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[
                {{"op":"color_matrix","matrix":[
                    1,0,0,0,0,
                    0,1,0,0,0,
                    0,0,1,0,0,
                    0,0,0,0.5,0]}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    // 255 → 0.5 → 127.5 → 128(half-away-from-zero)、200 → 100.0 → 100。
    let alphas: Vec<u8> = pixels(&out).iter().map(|p| p[3]).collect();
    assert_eq!(alphas, vec![128, 128, 128, 100, 128]);
    // RGB は恒等行なので不変。
    for (a, b) in pixels(&out).iter().zip(pixels(&src).iter()) {
        assert_eq!(a[0..3], b[0..3]);
    }
}

/// 行列は 0..1 でクランプされる(オーバーフローで巻き戻らない)。
#[test]
fn color_matrix_clamps_out_of_range() {
    let src = probe_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[
                {{"op":"color_matrix","matrix":[
                    4,0,0,0,0.5,
                    0,4,0,0,-0.5,
                    0,0,4,0,0,
                    0,0,0,1,0]}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    // 赤(255,0,0): R' = 4*1 + 0.5 = 4.5 → clamp 1 → 255、G' = -0.5 → clamp 0 → 0。
    // 巻き戻り(wrap)していれば 255 以外の値になる。
    assert_eq!(pixels(&out)[0], [255, 0, 0, 255]);
    // 緑(0,255,0): R' = 0.5 → 127.5 → 128、G' = 4 - 0.5 = 3.5 → clamp 1 → 255。
    assert_eq!(pixels(&out)[1], [128, 255, 0, 255]);
}

// -------------------------------------------------------------------- curves

/// 恒等カーブ([[0,0],[255,255]])は1バイトも変えない。
#[test]
fn curves_identity_is_byte_identical() {
    let src = ramp_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[
                {{"op":"curves","master":[[0,0],[255,255]]}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    assert_eq!(out.as_raw(), src.as_raw());
}

/// curves はアルファに触れない。
#[test]
fn curves_never_touches_alpha() {
    let src = probe_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[
                {{"op":"curves","master":[[0,255],[255,0]]}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    let alphas: Vec<u8> = pixels(&out).iter().map(|p| p[3]).collect();
    assert_eq!(alphas, vec![255, 255, 255, 200, 255]);
}

/// 制御点1個は定数 LUT(全域をその y で塗り潰す)。
#[test]
fn curves_single_point_is_constant() {
    let src = ramp_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[
                {{"op":"curves","master":[[128,77]]}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    for p in pixels(&out) {
        assert_eq!(&p[0..3], &[77, 77, 77]);
    }
}

/// 端の外挿は定数クランプ: 最初の点より左は最初の y、最後の点より右は最後の y。
#[test]
fn curves_clamps_outside_control_point_range() {
    let out = run(
        &ramp_image(),
        &format!(
            r#"{{"operations":[
                {{"op":"curves","master":[[10,20],[200,240]]}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    let px = pixels(&out);
    assert_eq!(px[0][0], 20);
    assert_eq!(px[9][0], 20);
    assert_eq!(px[200][0], 240);
    assert_eq!(px[255][0], 240);
    // 制御点そのものは通る(単調3次 Hermite は節点補間)。
    assert_eq!(px[10][0], 20);
}

/// 適用順は master → 各チャンネル。
/// master で 0→255 に反転してから red で 255→0 に反転すれば R は元に戻り、
/// G/B は反転したままになる。
#[test]
fn curves_master_applies_before_channel() {
    let src = ramp_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[
                {{"op":"curves","master":[[0,255],[255,0]],"red":[[0,255],[255,0]]}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    let px = pixels(&out);
    for (i, p) in px.iter().enumerate() {
        assert_eq!(p[0], i as u8, "R must be restored by the double inversion");
        assert_eq!(p[1], 255 - i as u8, "G is inverted by master only");
        assert_eq!(p[2], 255 - i as u8, "B is inverted by master only");
    }
}

/// チャンネル別カーブは指定チャンネルだけに効く。
#[test]
fn curves_channel_isolation() {
    let out = run(
        &ramp_image(),
        &format!(
            r#"{{"operations":[
                {{"op":"curves","green":[[0,0],[128,64],[255,255]]}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    let px = pixels(&out);
    for (i, p) in px.iter().enumerate() {
        assert_eq!(p[0], i as u8, "R untouched");
        assert_eq!(p[2], i as u8, "B untouched");
    }
    assert_eq!(px[0][1], 0);
    assert_eq!(px[128][1], 64, "control point is interpolated exactly");
    assert_eq!(px[255][1], 255);
    assert!(px[64][1] < 64, "the curve pulls midtones down");
}

// -------------------------------------------------------------------- levels

/// 既定値の levels は恒等。
#[test]
fn levels_identity_is_byte_identical() {
    let src = ramp_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[
                {{"op":"levels","in_black":0,"in_white":255,"gamma":1.0,
                  "out_black":0,"out_white":255}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    assert_eq!(out.as_raw(), src.as_raw());
}

/// serde の既定値(in_white=255, gamma=1.0, out_white=255)だけでも恒等。
#[test]
fn levels_defaults_are_identity() {
    let src = ramp_image();
    let out = run(
        &src,
        &format!(r#"{{"operations":[{{"op":"levels"}},{PNG_ENCODE}]}}"#),
    );
    assert_eq!(out.as_raw(), src.as_raw());
}

/// levels はアルファに触れない。
#[test]
fn levels_never_touches_alpha() {
    let src = probe_image();
    let out = run(
        &src,
        &format!(
            r#"{{"operations":[
                {{"op":"levels","in_black":32,"in_white":224,"gamma":2.0}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    let alphas: Vec<u8> = pixels(&out).iter().map(|p| p[3]).collect();
    assert_eq!(alphas, vec![255, 255, 255, 200, 255]);
}

/// gamma の既知値プローブ。
/// out = round((v/255)^(1/gamma) * 255)。gamma > 1 で明るく、< 1 で暗くなる。
#[test]
fn levels_gamma_known_values() {
    let bright = run(
        &ramp_image(),
        &format!(
            r#"{{"operations":[
                {{"op":"levels","in_black":0,"in_white":255,"gamma":2.0,
                  "out_black":0,"out_white":255}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    let px = pixels(&bright);
    // (i/255)^0.5 * 255 を手計算した値。
    assert_eq!(px[0][0], 0);
    assert_eq!(px[1][0], 16);
    assert_eq!(px[32][0], 90);
    assert_eq!(px[64][0], 128);
    assert_eq!(px[128][0], 181);
    assert_eq!(px[192][0], 221);
    assert_eq!(px[255][0], 255);

    let dark = run(
        &ramp_image(),
        &format!(
            r#"{{"operations":[
                {{"op":"levels","in_black":0,"in_white":255,"gamma":0.5,
                  "out_black":0,"out_white":255}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    let px = pixels(&dark);
    // (i/255)^2 * 255。
    assert_eq!(px[0][0], 0);
    assert_eq!(px[64][0], 16);
    assert_eq!(px[128][0], 64);
    assert_eq!(px[192][0], 145);
    assert_eq!(px[255][0], 255);
}

/// in レンジのクリップ(黒潰し・白飛ばし)。
#[test]
fn levels_input_range_clips() {
    let out = run(
        &ramp_image(),
        &format!(
            r#"{{"operations":[
                {{"op":"levels","in_black":32,"in_white":224,"gamma":1.0,
                  "out_black":0,"out_white":255}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    let px = pixels(&out);
    assert_eq!(px[0][0], 0);
    assert_eq!(px[32][0], 0, "in_black maps to out_black");
    assert_eq!(px[224][0], 255, "in_white maps to out_white");
    assert_eq!(px[255][0], 255);
    // (64-32)/192 * 255 = 42.5 → 43(half-away-from-zero)。
    assert_eq!(px[64][0], 43);
    assert_eq!(px[128][0], 128);
}

/// out レンジの圧縮。out_black == out_white は単色塗り潰し(合法)。
#[test]
fn levels_output_range_compresses() {
    let out = run(
        &ramp_image(),
        &format!(
            r#"{{"operations":[
                {{"op":"levels","in_black":0,"in_white":255,"gamma":1.0,
                  "out_black":100,"out_white":100}},
                {PNG_ENCODE}
            ]}}"#
        ),
    );
    for p in pixels(&out) {
        assert_eq!(&p[0..3], &[100, 100, 100]);
    }
}

// ------------------------------------------------------------------ validate

fn expect_invalid(json: &str, needle: &str) {
    let src = probe_image();
    let err = apply_recipe(&encode_png(&src), &recipe(json), &Limits::default())
        .expect_err("recipe must be rejected");
    match &err {
        AtxError::InvalidRecipe(msg) => assert!(
            msg.contains(needle),
            "error message {msg:?} should mention {needle:?}"
        ),
        other => panic!("expected InvalidRecipe, got {other:?}"),
    }
}

#[test]
fn validate_rejects_bad_matrix_length() {
    expect_invalid(
        r#"{"operations":[{"op":"color_matrix","matrix":[1,0,0,0,0]}]}"#,
        "20 elements",
    );
}

#[test]
fn validate_rejects_non_finite_matrix_value() {
    // JSON に Infinity/NaN リテラルは無いので、巨大値の代わりに範囲外で拾う経路と
    // 非有限値の経路の両方を確認する(非有限は構築した f64 を直接 serde で渡す)。
    let mut matrix = vec![0.0f64; 20];
    matrix[0] = f64::NAN;
    let r = TransformRecipe {
        layers: None,
        operations: vec![atx_core::Operation::ColorMatrix { matrix, mask: None }],
    };
    let err = apply_recipe(&encode_png(&probe_image()), &r, &Limits::default())
        .expect_err("NaN matrix must be rejected");
    assert!(
        matches!(&err, AtxError::InvalidRecipe(m) if m.contains("finite")),
        "{err}"
    );
}

#[test]
fn validate_rejects_out_of_range_matrix_value() {
    expect_invalid(
        r#"{"operations":[{"op":"color_matrix","matrix":[
            9,0,0,0,0, 0,1,0,0,0, 0,0,1,0,0, 0,0,0,1,0]}]}"#,
        "matrix[0]",
    );
}

#[test]
fn validate_rejects_curves_with_no_channel() {
    expect_invalid(
        r#"{"operations":[{"op":"curves"}]}"#,
        "at least one of master",
    );
}

#[test]
fn validate_rejects_duplicate_curve_x() {
    // メッセージには op index と衝突した x が含まれる。
    expect_invalid(
        r#"{"operations":[
            {"op":"auto_orient"},
            {"op":"curves","master":[[0,0],[128,10],[128,200],[255,255]]}
        ]}"#,
        "operations[1] (curves): master control point x values must be strictly increasing, \
         got 128 after 128",
    );
}

#[test]
fn validate_rejects_decreasing_curve_x() {
    expect_invalid(
        r#"{"operations":[{"op":"curves","red":[[0,0],[200,10],[100,200]]}]}"#,
        "got 100 after 200",
    );
}

#[test]
fn validate_rejects_empty_curve_channel() {
    expect_invalid(
        r#"{"operations":[{"op":"curves","blue":[]}]}"#,
        "must have 1..=32 control points",
    );
}

#[test]
fn validate_rejects_too_many_curve_points() {
    let points: Vec<String> = (0..33).map(|i| format!("[{},{}]", i * 7, i)).collect();
    expect_invalid(
        &format!(
            r#"{{"operations":[{{"op":"curves","master":[{}]}}]}}"#,
            points.join(",")
        ),
        "must have 1..=32 control points",
    );
}

#[test]
fn validate_rejects_levels_inverted_input_range() {
    expect_invalid(
        r#"{"operations":[{"op":"levels","in_black":200,"in_white":100}]}"#,
        "in_black must be less than in_white",
    );
    expect_invalid(
        r#"{"operations":[{"op":"levels","in_black":128,"in_white":128}]}"#,
        "in_black must be less than in_white",
    );
}

#[test]
fn validate_rejects_levels_inverted_output_range() {
    expect_invalid(
        r#"{"operations":[{"op":"levels","out_black":200,"out_white":100}]}"#,
        "out_black must be less than or equal to out_white",
    );
}

#[test]
fn validate_rejects_levels_gamma_out_of_range() {
    expect_invalid(
        r#"{"operations":[{"op":"levels","gamma":0.05}]}"#,
        "gamma must be a finite value within 0.1..=10",
    );
    expect_invalid(
        r#"{"operations":[{"op":"levels","gamma":12.0}]}"#,
        "gamma must be a finite value within 0.1..=10",
    );
}

// ------------------------------------------------------------------ 決定論

/// 同じ入力・同じレシピを2回流したらバイト同一(DESIGN §6「決定論」)。
#[test]
fn color_ops_are_deterministic() {
    let src = probe_image();
    let recipes = [
        format!(
            r#"{{"operations":[{{"op":"color_matrix","matrix":[
                0.393,0.769,0.189,0,0, 0.349,0.686,0.168,0,0,
                0.272,0.534,0.131,0,0, 0,0,0,1,0]}},{PNG_ENCODE}]}}"#
        ),
        format!(
            r#"{{"operations":[{{"op":"curves","master":[[0,12],[64,40],[192,220],[255,250]],
                "red":[[0,0],[128,140],[255,255]]}},{PNG_ENCODE}]}}"#
        ),
        format!(
            r#"{{"operations":[{{"op":"levels","in_black":16,"in_white":240,"gamma":1.7,
                "out_black":8,"out_white":250}},{PNG_ENCODE}]}}"#
        ),
    ];
    for json in &recipes {
        let a = run_bytes(&src, json);
        let b = run_bytes(&src, json);
        assert_eq!(a, b, "same recipe must produce byte-identical output");
    }
}

// ------------------------------------------------------------------ ゴールデン

/// フルパイプラインのゴールデン(curves + color_matrix + JPEG エンコード)。
///
/// 合成フィクスチャ `tests/fixtures/synthetic_scene.jpg`
/// (`cargo run -p atx-core --example gen_fixture` で再生成可能)に対して
/// 出力 sha256 をピン留めする。engine.rs のゴールデンと同じ規律:
/// **意図的にアルゴリズムを変えた場合のみ `ENGINE_VERSION` を上げた上で更新すること**。
/// フィクスチャを作り直した場合もこの値の更新が必要。
#[test]
fn golden_curves_and_color_matrix_pipeline_sha256() {
    let r = recipe(
        r#"{"operations":[
            {"op":"curves","master":[[0,8],[64,56],[192,208],[255,250]],
             "blue":[[0,0],[128,120],[255,255]]},
            {"op":"color_matrix","matrix":[
                0.393,0.769,0.189,0,0,
                0.349,0.686,0.168,0,0,
                0.272,0.534,0.131,0,0,
                0,0,0,1,0]},
            {"op":"encode","format":"jpeg","quality":85}
        ]}"#,
    );
    let out = apply_recipe(FIXTURE, &r, &Limits::default()).unwrap();
    assert_eq!(atx_core::ENGINE_VERSION, "atx-core/2");
    assert_eq!(
        sha256_hex(&out.bytes),
        // v2 (f32 linear) golden; v1 value was a1ea7e7aaec270d0e73cbd095f97f4d068343e491179259fea7f69e5d21a3538
        "58fb0f6df4ac70a71764edfd71ea7a3d4c8cf7aa729855a0781e17512f590ba8",
        "color op golden"
    );
}

// ------------------------------------------------------------------ proptest

mod props {
    use super::*;
    use proptest::prelude::*;

    /// 単調非減少な制御点列を生成する(x は狭義単調増加、y は非減少)。
    fn monotone_points() -> impl Strategy<Value = Vec<[u8; 2]>> {
        proptest::collection::vec((0u8..=255u8, 0u8..=255u8), 2..10).prop_map(|raw| {
            let mut xs: Vec<u8> = raw.iter().map(|(x, _)| *x).collect();
            xs.sort_unstable();
            xs.dedup();
            let mut ys: Vec<u8> = raw.iter().map(|(_, y)| *y).collect();
            ys.sort_unstable();
            ys.truncate(xs.len());
            xs.truncate(ys.len());
            xs.into_iter().zip(ys).map(|(x, y)| [x, y]).collect()
        })
    }

    proptest! {
        /// 単調非減少な制御点から作られた LUT は単調非減少(Fritsch–Carlson の保証)。
        /// LUT は 0..255 のランプ画像を通した出力としてそのまま読み出せる。
        #[test]
        fn monotone_control_points_give_monotone_lut(points in monotone_points()) {
            let json = format!(
                r#"{{"operations":[{{"op":"curves","master":{}}},{}]}}"#,
                serde_json::to_string(&points).unwrap(),
                PNG_ENCODE
            );
            let out = run(&ramp_image(), &json);
            let lut: Vec<u8> = out.pixels().map(|p| p.0[0]).collect();
            for w in lut.windows(2) {
                prop_assert!(
                    w[1] >= w[0],
                    "LUT must be monotone non-decreasing, got {:?} then {:?} for points {:?}",
                    w[0], w[1], points
                );
            }
        }

        /// levels の LUT も(out_black <= out_white なので)単調非減少。
        #[test]
        fn levels_lut_is_monotone(
            in_black in 0u8..=200,
            span in 1u8..=55,
            gamma in 0.1f64..=10.0,
            out_black in 0u8..=200,
            out_span in 0u8..=55,
        ) {
            let in_white = in_black.saturating_add(span).max(in_black + 1);
            let out_white = out_black.saturating_add(out_span);
            let json = format!(
                r#"{{"operations":[{{"op":"levels","in_black":{in_black},"in_white":{in_white},
                    "gamma":{gamma},"out_black":{out_black},"out_white":{out_white}}},{}]}}"#,
                PNG_ENCODE
            );
            let out = run(&ramp_image(), &json);
            let lut: Vec<u8> = out.pixels().map(|p| p.0[0]).collect();
            for w in lut.windows(2) {
                prop_assert!(w[1] >= w[0]);
            }
            prop_assert!(lut.iter().all(|&v| v >= out_black && v <= out_white));
        }

        /// color_matrix は決定論的(同じ入力から2回同じ出力)。
        #[test]
        fn color_matrix_is_deterministic(k in -2.0f64..=2.0) {
            let json = format!(
                r#"{{"operations":[{{"op":"color_matrix","matrix":[
                    {k},0,0,0,0, 0,{k},0,0,0, 0,0,{k},0,0, 0,0,0,1,0]}},{}]}}"#,
                PNG_ENCODE
            );
            let src = probe_image();
            prop_assert_eq!(run_bytes(&src, &json), run_bytes(&src, &json));
        }
    }
}
