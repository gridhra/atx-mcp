//! 派生 revision(= 一度エンコードした自前の出力)へさらに変換を適用する経路の回帰テスト。
//!
//! 実利用のバグ報告: `rotate(original) -> rev_A` は正しいのに、
//! `crop(rev_A のバイト列) -> rev_B` が緑の縞で壊れる、というもの。
//!
//! ## 不変条件の選び方
//!
//! - **PNG 中間(可逆)**: 2段階適用と1段階適用は「同じ画素に同じ op を同じ順で
//!   かけたもの」なのでバイト同一でなければならない。→ 厳密一致を検証する。
//! - **JPEG 中間(非可逆)**: 世代劣化があるためバイト同一は保証されない。
//!   代わりに (a) チャンネル毎の平均絶対差が小さいこと、(b) チャンネル平均が
//!   1段階版と近いこと(= 色プレーン入れ替え / stride ずれの検出)を検証する。

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe, Limits};
use image::RgbImage;

const FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");

fn recipe(json: &str) -> TransformRecipe {
    serde_json::from_str(json).expect("recipe should parse")
}

fn apply(bytes: &[u8], json: &str) -> atx_core::EncodedOutput {
    apply_recipe(bytes, &recipe(json), &Limits::default()).expect("apply_recipe should succeed")
}

fn decode_rgb(bytes: &[u8]) -> RgbImage {
    image::load_from_memory(bytes)
        .expect("output should decode")
        .to_rgb8()
}

/// チャンネル毎の平均値 [r, g, b]。
fn channel_means(img: &RgbImage) -> [f64; 3] {
    let mut sums = [0f64; 3];
    for p in img.pixels() {
        for (s, v) in sums.iter_mut().zip(p.0.iter()) {
            *s += *v as f64;
        }
    }
    let n = (img.width() as f64) * (img.height() as f64);
    [sums[0] / n, sums[1] / n, sums[2] / n]
}

/// チャンネル毎の平均絶対差。
fn mean_abs_diff(a: &RgbImage, b: &RgbImage) -> [f64; 3] {
    assert_eq!(a.dimensions(), b.dimensions(), "dimensions must match");
    let mut sums = [0f64; 3];
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        for (s, (va, vb)) in sums.iter_mut().zip(pa.0.iter().zip(pb.0.iter())) {
            *s += (*va as f64 - *vb as f64).abs();
        }
    }
    let n = (a.width() as f64) * (a.height() as f64);
    [sums[0] / n, sums[1] / n, sums[2] / n]
}

const ROTATE: &str = r#"{"op":"rotate","angle_degrees":-1.8,"crop":"largest_inscribed_rect"}"#;
const CROP: &str = r#"{"op":"crop","aspect_ratio":"16:9","anchor":"center"}"#;

/// PNG 中間は可逆なので、2段階と1段階はバイト同一でなければならない。
#[test]
fn two_stage_png_matches_single_shot_exactly() {
    let stage1 = apply(
        FIXTURE,
        &format!(r#"{{"operations":[{ROTATE},{{"op":"encode","format":"png"}}]}}"#),
    );
    let stage2 = apply(
        &stage1.bytes,
        &format!(r#"{{"operations":[{CROP},{{"op":"encode","format":"png"}}]}}"#),
    );
    let single = apply(
        FIXTURE,
        &format!(r#"{{"operations":[{ROTATE},{CROP},{{"op":"encode","format":"png"}}]}}"#),
    );

    assert_eq!(
        (stage2.width, stage2.height),
        (single.width, single.height),
        "two-stage and single-shot must agree on dimensions"
    );
    assert_eq!(
        stage2.bytes, single.bytes,
        "lossless (png) intermediate: two-stage must be bit-identical to single-shot"
    );
}

/// 自前の JPEG 出力は、自前のデコーダ(`image` = パイプラインの入力側)で
/// 元画素どおりに読み戻せなければならない。
///
/// 退行の実体: `jpeg-encoder` の最適化ハフマンテーブルは非インターリーブ多重スキャンを
/// 生成し、それを `image` 0.25 (zune-jpeg) がクロマ抜けで復号していた
/// (= 緑一色 + 横縞)。品質 < 90 で自動的に 4:2:0 になるため、既定品質が直撃していた。
#[test]
fn our_own_jpeg_output_decodes_back_to_the_same_picture() {
    // 既定品質(= encode op で quality を省略した場合)を必ず含める。
    for quality in [
        "",
        r#","quality":75"#,
        r#","quality":85"#,
        r#","quality":95"#,
    ] {
        let out = apply(
            FIXTURE,
            &format!(r#"{{"operations":[{{"op":"encode","format":"jpeg"{quality}}}]}}"#),
        );
        let src = image::load_from_memory(FIXTURE).unwrap().to_rgb8();
        let back = decode_rgb(&out.bytes);
        assert_eq!(back.dimensions(), src.dimensions());

        let ms = channel_means(&src);
        let mb = channel_means(&back);
        for i in 0..3 {
            assert!(
                (ms[i] - mb[i]).abs() < 3.0,
                "quality {quality:?}: channel {i} mean {:.2} != source {:.2}; \
                 means {mb:?} vs {ms:?} (a green cast here means the encoder emitted a \
                 non-interleaved multi-scan jpeg that our own decoder cannot read)",
                mb[i],
                ms[i]
            );
        }
    }
}

/// JPEG 中間は非可逆なので知覚的な健全性のみを検証する。
/// 特に「緑の縞」= チャンネル取り違え / stride ずれが起きていないことを見る。
/// quality は既定(省略)にして、実運用と同じ経路を通す。
#[test]
fn two_stage_jpeg_is_perceptually_equivalent_to_single_shot() {
    let stage1 = apply(
        FIXTURE,
        &format!(r#"{{"operations":[{ROTATE},{{"op":"encode","format":"jpeg"}}]}}"#),
    );
    let stage2 = apply(
        &stage1.bytes,
        &format!(r#"{{"operations":[{CROP},{{"op":"encode","format":"jpeg"}}]}}"#),
    );
    let single = apply(
        FIXTURE,
        &format!(r#"{{"operations":[{ROTATE},{CROP},{{"op":"encode","format":"jpeg"}}]}}"#),
    );

    assert_eq!((stage2.width, stage2.height), (single.width, single.height));

    let a = decode_rgb(&stage2.bytes);
    let b = decode_rgb(&single.bytes);

    // (a) 世代劣化ぶんの差しか無いこと。
    // 既定品質(85 = 4:2:0)の JPEG を2回通すぶんの実測値は各チャンネル 3.0〜3.5 程度。
    // 壊れている場合は 2 桁になるので、6.0 で十分に分離できる。
    let diff = mean_abs_diff(&a, &b);
    for (i, d) in diff.iter().enumerate() {
        assert!(
            *d < 6.0,
            "channel {i}: mean abs diff {d:.3} is too large (generation loss should be small); \
             full diff {diff:?}"
        );
    }

    // (b) チャンネル平均が近いこと(色プレーン入れ替え / 緑被りの検出)。
    let ma = channel_means(&a);
    let mb = channel_means(&b);
    for i in 0..3 {
        let rel = (ma[i] - mb[i]).abs() / mb[i].max(1.0);
        assert!(
            rel < 0.02,
            "channel {i} mean drifted: two-stage {:.2} vs single-shot {:.2} ({:.1}%); \
             means {ma:?} vs {mb:?}",
            ma[i],
            mb[i],
            rel * 100.0
        );
    }
}

/// バグ報告そのままの再現形: encode op を書かない(= 入力フォーマット継承)3段。
/// rev_A(jpeg)を再入力にした rev_B が壊れていないことを見る。
#[test]
fn reported_repro_without_explicit_encode_op() {
    let rev_a = apply(FIXTURE, &format!(r#"{{"operations":[{ROTATE}]}}"#));
    assert_eq!(rev_a.mime_type, "image/jpeg");
    let rev_b = apply(&rev_a.bytes, &format!(r#"{{"operations":[{CROP}]}}"#));
    let rev_c = apply(FIXTURE, &format!(r#"{{"operations":[{ROTATE},{CROP}]}}"#));

    let b = decode_rgb(&rev_b.bytes);
    let c = decode_rgb(&rev_c.bytes);
    let mb = channel_means(&b);
    let mc = channel_means(&c);
    for i in 0..3 {
        let rel = (mb[i] - mc[i]).abs() / mc[i].max(1.0);
        assert!(
            rel < 0.02,
            "channel {i}: derived-revision output drifted ({:.2} vs {:.2}); means {mb:?} vs {mc:?}",
            mb[i],
            mc[i]
        );
    }
}

/// `render_preview` と同じ形(レシピ + resize contain 768 + jpeg q80)を
/// 派生 revision に対して実行しても壊れない。
#[test]
fn preview_shaped_recipe_on_a_derived_revision() {
    const PREVIEW_TAIL: &str = r#"{"op":"resize","width":768,"height":768,"fit":"contain"},{"op":"encode","format":"jpeg","quality":80}"#;

    let rev_a = apply(FIXTURE, &format!(r#"{{"operations":[{ROTATE}]}}"#));
    let from_derived = apply(
        &rev_a.bytes,
        &format!(r#"{{"operations":[{CROP},{PREVIEW_TAIL}]}}"#),
    );
    let from_original = apply(
        FIXTURE,
        &format!(r#"{{"operations":[{ROTATE},{CROP},{PREVIEW_TAIL}]}}"#),
    );
    assert_eq!(
        (from_derived.width, from_derived.height),
        (from_original.width, from_original.height)
    );

    let a = decode_rgb(&from_derived.bytes);
    let b = decode_rgb(&from_original.bytes);
    let diff = mean_abs_diff(&a, &b);
    for (i, d) in diff.iter().enumerate() {
        assert!(
            *d < 3.0,
            "preview channel {i} mean abs diff {d:.3}; {diff:?}"
        );
    }
}

/// 自前の PNG 出力(アルファ付き)を再入力にしても壊れない。
/// アルファ付き中間 → JPEG 出力という「RGBA を持つ派生 revision」経路。
#[test]
fn rgba_png_intermediate_survives_a_second_pass() {
    // pad モードで透明パディングを入れて alpha を持つ PNG を作る。
    let stage1 = apply(
        FIXTURE,
        r##"{"operations":[
            {"op":"resize","width":300,"fit":"contain"},
            {"op":"crop","aspect_ratio":"1:1","mode":"pad","pad_color":"#00000000"},
            {"op":"encode","format":"png"}
        ]}"##,
    );
    let info = atx_core::inspect_bytes(&stage1.bytes, &Limits::default()).unwrap();
    assert!(info.has_alpha, "stage1 png should carry alpha");

    let stage2 = apply(
        &stage1.bytes,
        r#"{"operations":[{"op":"crop","aspect_ratio":"16:9"},{"op":"encode","format":"png"}]}"#,
    );
    let single = apply(
        FIXTURE,
        r##"{"operations":[
            {"op":"resize","width":300,"fit":"contain"},
            {"op":"crop","aspect_ratio":"1:1","mode":"pad","pad_color":"#00000000"},
            {"op":"crop","aspect_ratio":"16:9"},
            {"op":"encode","format":"png"}
        ]}"##,
    );
    assert_eq!(
        stage2.bytes, single.bytes,
        "rgba png intermediate: two-stage must be bit-identical to single-shot"
    );
}

/// WebP 中間(こちらも自前出力)からの再適用。
#[test]
fn webp_intermediate_keeps_channel_order() {
    let stage1 = apply(
        FIXTURE,
        &format!(r#"{{"operations":[{ROTATE},{{"op":"encode","format":"webp","quality":95}}]}}"#),
    );
    let stage2 = apply(
        &stage1.bytes,
        &format!(r#"{{"operations":[{CROP},{{"op":"encode","format":"png"}}]}}"#),
    );
    let single = apply(
        FIXTURE,
        &format!(r#"{{"operations":[{ROTATE},{CROP},{{"op":"encode","format":"png"}}]}}"#),
    );

    let a = decode_rgb(&stage2.bytes);
    let b = decode_rgb(&single.bytes);
    let ma = channel_means(&a);
    let mb = channel_means(&b);
    for i in 0..3 {
        let rel = (ma[i] - mb[i]).abs() / mb[i].max(1.0);
        assert!(
            rel < 0.03,
            "channel {i} mean drifted through webp intermediate: {:.2} vs {:.2}; {ma:?} vs {mb:?}",
            ma[i],
            mb[i]
        );
    }
}
