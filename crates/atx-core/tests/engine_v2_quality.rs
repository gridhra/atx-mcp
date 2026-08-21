//! 画素エンジン v2(f32 リニアライト)の**品質の証明**。
//!
//! v0.4 は唯一の破壊的リリースであり、その存在理由は「出力が変わったこと」ではなく
//! **「v1 では原理的に不可能だった精度・物理的正しさが手に入ったこと」**にある。
//! このファイルはその 3 点を、v1 では必ず落ちる形の主張として固定する。
//!
//! 1. `tone_stack_of_eight_curves_is_byte_identical`
//!    — トーン系 op を重ねてもポスタリゼーションが蓄積しない(f32 が精度を運ぶ)。
//!    v1 は op ごとに u8 へ丸めていたため、往復すると必ず階調が欠けた。
//! 2. `linear_downscale_of_checkerboard_averages_in_light`
//!    — 白黒市松の縮小は **線形光で 0.5**(= sRGB 約 188)になる。
//!    v1 は符号値を平均していたため sRGB 128(= 線形 0.216)という、
//!    物理的には「暗すぎる」古典的なガンマ・ブラー誤差を出していた。
//! 3. `full_eighteen_op_pipeline_is_deterministic`
//!    — 全 18 op を通したパイプラインが 2 回実行でバイト同一(決定論の回帰)。
//!
//! さらに v0.4 で入った 16bit I/O(PNG16)の往復も検証する。

use std::collections::HashMap;

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe, apply_recipe_with_assets, AssetResolver, Limits, Result};
use image::{ImageBuffer, ImageEncoder, Rgba, RgbaImage};

const IDENTITY_8: &str = include_str!("../../../tests/fixtures/identity_8.cube");

// ---------------------------------------------------------------------------
// ヘルパ
// ---------------------------------------------------------------------------

struct MockAssets(HashMap<String, Vec<u8>>);

impl AssetResolver for MockAssets {
    fn read_revision(&self, revision_id: &str) -> Result<Vec<u8>> {
        self.0.get(revision_id).cloned().ok_or_else(|| {
            atx_core::AtxError::InvalidRecipe(format!("unknown revision {revision_id}"))
        })
    }
}

fn recipe(json: &str) -> TransformRecipe {
    serde_json::from_str(json).expect("recipe should parse")
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

fn apply(bytes: &[u8], json: &str) -> atx_core::EncodedOutput {
    apply_recipe(bytes, &recipe(json), &Limits::default()).expect("apply_recipe should succeed")
}

/// sRGB OETF(線形光 → 符号値 0..1)。期待値を物理から立てるために使う。
fn oetf(l: f64) -> f64 {
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

// ---------------------------------------------------------------------------
// (a) トーンスタックの精度
// ---------------------------------------------------------------------------

/// 全 u8 値を含むグラデーション(256x64、R/G/B で位相をずらす)。
fn gradient_image() -> RgbaImage {
    RgbaImage::from_fn(256, 64, |x, y| {
        Rgba([
            x as u8,
            ((x + y) % 256) as u8,
            ((x * 3 + y * 5) % 256) as u8,
            255,
        ])
    })
}

/// **交互に 8 回かけて元に戻るトーンスタックは、入力とバイト同一で返る。**
///
/// 使うのは 2 制御点の直線カーブ 2 本:
/// - `up`   : `(0,32) → (255,255)`  つまり `y = 32 + x * 223/255`
/// - `down` : `(32,0) → (255,255)`  つまり `y = (x - 32) * 255/223`(x < 32 は 0)
///
/// 制御点が 2 個なら Fritsch–Carlson の接線は両端とも区間傾きに一致するので、
/// Hermite 3 次は**厳密に直線**になる。したがって `down ∘ up` は数学的に恒等であり、
/// それを 4 セット(= 8 op)重ねる。
///
/// # なぜこれが v2 の証明になるのか
///
/// v1 の curves は `[u8; 256]` の LUT を引いて **op ごとに u8 へ丸めて**いた。
/// `up` は 256 段を 223 段へ圧縮するので、その時点で 33 段ぶんの情報が失われ、
/// `down` で引き伸ばしても戻らない(階調が櫛の歯状に欠ける = ポスタリゼーション)。
/// 8 op も重ねれば目視で分かるレベルの縞になる。
///
/// v2 では LUT のノード値が f32(0..1)、ノード間は f32 線形補間で、
/// 中間結果は一度も量子化されない。さらにエンジンは作業空間を遅延追跡するので、
/// 連続する 8 個の sRGB 空間 op に対して伝達関数の往復は **0 回**である。
/// 結果、丸めは出口の 1 回だけになり、入力バイト列がそのまま返る。
#[test]
fn tone_stack_of_eight_curves_is_byte_identical() {
    let input = encode_png(&gradient_image());

    // 何もしない基準(デコード → エンコードのみ)。
    let baseline = apply(&input, r#"{"operations":[{"op":"encode","format":"png"}]}"#);

    const UP: &str = r#"{"op":"curves","master":[[0,32],[255,255]]}"#;
    const DOWN: &str = r#"{"op":"curves","master":[[32,0],[255,255]]}"#;
    let ops = [UP, DOWN, UP, DOWN, UP, DOWN, UP, DOWN].join(",");
    let stacked = apply(
        &input,
        &format!(r#"{{"operations":[{ops},{{"op":"encode","format":"png"}}]}}"#),
    );

    assert_eq!(
        stacked.bytes, baseline.bytes,
        "8 alternating net-identity curves must return the input byte-for-byte \
         (v1 would have posterized: up compresses 256 levels into 223 and u8 rounding \
          made that loss permanent)"
    );

    // 念のため画素でも確認する(PNG のメタデータが偶然一致しただけ、を排除)。
    let out = decode_rgba(&stacked.bytes);
    let src = gradient_image();
    for (a, b) in out.pixels().zip(src.pixels()) {
        assert_eq!(a.0, b.0, "pixel drifted through the tone stack");
    }
}

// ---------------------------------------------------------------------------
// (b) 線形光での縮小
// ---------------------------------------------------------------------------

/// 2px セルの白黒市松(256x256)。
fn checkerboard() -> RgbaImage {
    RgbaImage::from_fn(256, 256, |x, y| {
        let on = ((x / 2) + (y / 2)) % 2 == 0;
        let v = if on { 255 } else { 0 };
        Rgba([v, v, v, 255])
    })
}

/// **白黒市松を 8 倍縮小すると、平均は「線形光で 0.5」= sRGB 約 188 になる。**
///
/// # 物理
///
/// 黒(線形 0.0)と白(線形 1.0)が面積比 1:1 で混ざった領域から返ってくる光量は
/// 定義により 0.5 である。それを sRGB で符号化すると
/// `OETF(0.5) = 1.055 * 0.5^(1/2.4) - 0.055 ≈ 0.7354` → u8 で **188**。
///
/// v1 は符号値(0 と 255)をそのまま平均して 128 を返していた。
/// 128 は線形光では 0.216 — つまり本来の 43% しか光がない、**暗すぎる**答えだった。
/// これが「縮小・ぼかしをすると画像が沈む」という、ガンマを無視した実装に共通の
/// 古典的な誤差である(Adobe/ImageMagick が "gamma-correct resize" と呼ぶもの)。
///
/// 端の画素は Lanczos3 のクランプ外挿で市松の平均から外れるため、内側だけを見る。
#[test]
fn linear_downscale_of_checkerboard_averages_in_light() {
    let input = encode_png(&checkerboard());
    let out = apply(
        &input,
        r#"{"operations":[
            {"op":"resize","width":32,"height":32,"fit":"fill","without_enlargement":false},
            {"op":"encode","format":"png"}
        ]}"#,
    );
    assert_eq!((out.width, out.height), (32, 32));
    let img = decode_rgba(&out.bytes);

    let expected = (oetf(0.5) * 255.0).round() as i32;
    assert_eq!(expected, 188, "OETF(0.5) must encode to sRGB 188");

    let mut sum = 0i64;
    let mut count = 0i64;
    for y in 4..28u32 {
        for x in 4..28u32 {
            sum += img.get_pixel(x, y).0[0] as i64;
            count += 1;
        }
    }
    let mean = sum as f64 / count as f64;
    assert!(
        (mean - expected as f64).abs() <= 2.0,
        "mean of the linear-light downscale is {mean}, expected ~{expected} (sRGB). \
         A value near 128 would mean the engine is averaging encoded values again \
         (the gamma-blur error v0.4 exists to remove)."
    );
    assert!(
        mean > 170.0,
        "mean {mean} is far too dark to be a linear-light average"
    );
}

// ---------------------------------------------------------------------------
// (c) 決定論(全 18 op)
// ---------------------------------------------------------------------------

/// 18 op すべてを 1 本に並べたレシピ(`op_name` の全分岐を踏む)。
fn all_ops_recipe() -> String {
    [
        r#"{"op":"auto_orient"}"#,
        r#"{"op":"perspective","vertical_degrees":2.0}"#,
        r#"{"op":"rotate","angle_degrees":-1.8,"crop":"largest_inscribed_rect"}"#,
        r#"{"op":"crop","aspect_ratio":"16:9","anchor":"center"}"#,
        r#"{"op":"resize","width":320,"fit":"cover"}"#,
        r#"{"op":"white_balance","temperature":18.0,"tint":-6.0}"#,
        r#"{"op":"blur","sigma":0.8}"#,
        r#"{"op":"median","radius":1}"#,
        r#"{"op":"unsharp_mask","amount":0.7,"radius":1.2,"threshold":3}"#,
        r#"{"op":"convolve","kernel":[0,-1,0,-1,5,-1,0,-1,0],"size":3,"divisor":1.0,"offset":0.0}"#,
        r#"{"op":"levels","in_black":6,"in_white":250,"gamma":1.1,"out_black":2,"out_white":253}"#,
        r#"{"op":"curves","master":[[0,4],[128,140],[255,252]]}"#,
        r#"{"op":"color_matrix","matrix":[1.02,0.0,0.0,0,0,0.0,1.0,0.0,0,0,0.0,0.0,0.98,0,0,0,0,0,1,0]}"#,
        r#"{"op":"hsl","blue":{"hue":8.0,"saturation":12.0,"luminance":-4.0}}"#,
        r#"{"op":"lut","lut_revision_id":"rev_identity","strength":0.5}"#,
        r#"{"op":"adjust","brightness":0.03,"contrast":0.02,"saturation":0.04,"sharpness":0.15}"#,
        r#"{"op":"strip_metadata","scope":"exif"}"#,
        r#"{"op":"encode","format":"png"}"#,
    ]
    .join(",")
}

/// 18 op のフルパイプラインは 2 回実行してバイト同一(f32 化しても決定論は不変)。
///
/// f32 では丸め順序・再結合・FMA の混入が結果を変えうるので、
/// 「同じプロセスで 2 回」でも並列分割のスレッド数ゆらぎを踏む価値がある
/// (行分割は画素間の順序しか変えない、という規約の実測ゲート)。
/// クロスプラットフォーム(mac/Linux)の一致は CI のゴールデンが担保する。
#[test]
fn full_eighteen_op_pipeline_is_deterministic() {
    let assets = MockAssets(HashMap::from([(
        "rev_identity".to_string(),
        IDENTITY_8.as_bytes().to_vec(),
    )]));
    let json = format!(r#"{{"operations":[{}]}}"#, all_ops_recipe());
    let r = recipe(&json);
    assert_eq!(r.operations.len(), 18, "this test must cover all 18 ops");

    let scene = encode_png(&gradient_image());
    let first = apply_recipe_with_assets(&scene, &r, &Limits::default(), &assets).unwrap();
    let second = apply_recipe_with_assets(&scene, &r, &Limits::default(), &assets).unwrap();

    assert_eq!(
        first.bytes, second.bytes,
        "the 18-op pipeline must be byte-identical across runs"
    );
    assert_eq!((first.width, first.height), (second.width, second.height));
}

// ---------------------------------------------------------------------------
// (d) 16bit I/O(v0.4 で追加)
// ---------------------------------------------------------------------------

/// 16bit のグラデーション PNG(8bit では表現できない刻みを持たせる)。
fn png16_gradient() -> Vec<u8> {
    // 横 1024 px に 0..65535 を線形に敷く。8bit へ落とすと 4 px ごとに
    // 同じ値へ潰れるが、16bit なら全画素が異なる値を保てる。
    let w = 1024u32;
    let img: ImageBuffer<Rgba<u16>, Vec<u16>> = ImageBuffer::from_fn(w, 4, |x, _| {
        let v = (x * 65535 / (w - 1)) as u16;
        Rgba([v, v, v, 65535])
    });
    let mut raw = Vec::with_capacity(img.as_raw().len() * 2);
    for v in img.as_raw() {
        raw.extend_from_slice(&v.to_ne_bytes());
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&raw, w, 4, image::ExtendedColorType::Rgba16)
        .unwrap();
    out
}

/// `encode { format: png, bit_depth: 16 }` は 8bit へ潰さずに階調を保つ。
///
/// 同じ入力を 8bit で書き出したものと比べ、**残る相異なる値の数**で差を測る。
/// 8bit 出力は原理的に 256 段しか持てないのに対し、16bit 出力は入力の
/// 1024 段をほぼそのまま残す。
#[test]
fn png16_encode_preserves_more_than_eight_bits() {
    let input = png16_gradient();

    let out16 = apply(
        &input,
        r#"{"operations":[{"op":"encode","format":"png","bit_depth":16}]}"#,
    );
    let out8 = apply(&input, r#"{"operations":[{"op":"encode","format":"png"}]}"#);

    let decoded16 = image::load_from_memory(&out16.bytes).unwrap();
    assert!(
        matches!(
            decoded16.color(),
            image::ColorType::Rgb16 | image::ColorType::Rgba16
        ),
        "bit_depth 16 must produce a 16-bit PNG, got {:?}",
        decoded16.color()
    );
    let rgba16 = decoded16.to_rgba16();

    let distinct16: std::collections::BTreeSet<u16> = rgba16.pixels().map(|p| p.0[0]).collect();
    let distinct8: std::collections::BTreeSet<u8> =
        decode_rgba(&out8.bytes).pixels().map(|p| p.0[0]).collect();

    assert!(
        distinct8.len() <= 256,
        "an 8-bit PNG cannot hold more than 256 levels, got {}",
        distinct8.len()
    );
    assert!(
        distinct16.len() > 900,
        "the 16-bit path must keep the input's ~1024 distinct levels, got {} \
         (8-bit output kept {})",
        distinct16.len(),
        distinct8.len()
    );

    // 16bit 出力は入力の値をほぼ保つ(変換 LUT の誤差は最暗部で数 LSB。
    // crate::linear のモジュールドキュメントの精度見積りを参照)。
    let src = image::load_from_memory(&input).unwrap().to_rgba16();
    let mut worst = 0i64;
    for (a, b) in rgba16.pixels().zip(src.pixels()) {
        worst = worst.max((a.0[0] as i64 - b.0[0] as i64).abs());
    }
    assert!(
        worst <= 8,
        "16-bit roundtrip drifted by {worst} LSB, expected only the LUT interpolation error"
    );
}

/// 16bit 入力を **線形光の op(resize)** に通しても 8bit へ潰れない。
///
/// 空間非依存 op だけのレシピは符号値のまま素通りするので、
/// 65536 エントリの EOTF テーブル(u16 → 線形)を実際に踏むのはこちらの経路。
/// 縦 1px へ縮めても横方向は等倍のままなので、階調はほぼそのまま残るはず。
#[test]
fn png16_survives_a_linear_space_op() {
    let input = png16_gradient();
    let out = apply(
        &input,
        r#"{"operations":[
            {"op":"resize","width":1024,"height":1,"fit":"fill","without_enlargement":false},
            {"op":"encode","format":"png","bit_depth":16}
        ]}"#,
    );
    let img = image::load_from_memory(&out.bytes).unwrap().to_rgba16();
    let distinct: std::collections::BTreeSet<u16> = img.pixels().map(|p| p.0[0]).collect();
    assert!(
        distinct.len() > 900,
        "a linear-space op on 16-bit input must not collapse to 8-bit steps, got {}",
        distinct.len()
    );
}
