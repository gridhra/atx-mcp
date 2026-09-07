//! `trim` / `threshold`(ドキュメント前処理の 2 op。DESIGN.md §9.12)のテスト。
//!
//! 他の op テストと同じく、すべて `apply_recipe`(JSON レシピ)経由の end-to-end 検証
//! (recipe.rs の validate + engine.rs の dispatch + ops モジュールの画素演算を
//! まとめて回帰させる。`ops` は `pub(crate)` なので統合テストからは直接触れない)。
//!
//! 入出力は PNG(可逆)で、期待値は各 op の定義式・定義された矩形から手で決める
//! (実装の出力を貼り付けたものではない)。

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe, Limits};
use image::{ImageFormat, Rgba, RgbaImage};
use proptest::prelude::*;

fn recipe(json: &str) -> TransformRecipe {
    serde_json::from_str(json).expect("recipe should parse")
}

fn apply(bytes: &[u8], json: &str) -> atx_core::EncodedOutput {
    apply_recipe(bytes, &recipe(json), &Limits::default()).expect("apply_recipe should succeed")
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

fn decode_rgba(bytes: &[u8]) -> RgbaImage {
    image::load_from_memory(bytes)
        .expect("output should decode")
        .to_rgba8()
}

/// PNG 経由(可逆)でレシピを適用し、結果の RGBA8 を返す。
fn run(img: &RgbaImage, json: &str) -> RgbaImage {
    decode_rgba(&apply(&encode_png(img), json).bytes)
}

/// 一様背景 `bg` のキャンバスに `rect` の内容色 `fg` を置いた合成画像。
fn canvas_with_box(
    w: u32,
    h: u32,
    bg: [u8; 4],
    fg: [u8; 4],
    rect: (u32, u32, u32, u32),
) -> RgbaImage {
    let mut img = RgbaImage::from_pixel(w, h, Rgba(bg));
    let (x, y, rw, rh) = rect;
    for yy in y..y + rh {
        for xx in x..x + rw {
            img.put_pixel(xx, yy, Rgba(fg));
        }
    }
    img
}

const WHITE: [u8; 4] = [255, 255, 255, 255];
const BLACK: [u8; 4] = [0, 0, 0, 255];

// ---------------------------------------------------------------------------
// trim
// ---------------------------------------------------------------------------

/// 白背景 40x30 の中に (8, 6)-(23, 17) の黒い内容。trim はその外接矩形ちょうどを返す。
#[test]
fn trim_cuts_exactly_to_the_content_rect() {
    let img = canvas_with_box(40, 30, WHITE, BLACK, (8, 6, 16, 12));
    let out = run(&img, r#"{"operations":[{"op":"trim"}]}"#);
    assert_eq!(out.dimensions(), (16, 12));
    // 四隅が内容色 = 余白が 1 画素も残っていない。
    for (x, y) in [(0, 0), (15, 0), (0, 11), (15, 11)] {
        assert_eq!(*out.get_pixel(x, y), Rgba(BLACK), "at ({x}, {y})");
    }
}

/// padding は外接矩形の外側に残り、画像端でクランプされる。
#[test]
fn trim_padding_is_kept_outside_the_content_and_clamped_at_the_edges() {
    let img = canvas_with_box(40, 30, WHITE, BLACK, (8, 6, 16, 12));
    let out = run(&img, r#"{"operations":[{"op":"trim","padding":4}]}"#);
    assert_eq!(out.dimensions(), (24, 20));
    // 左上 (4, 2) が背景、(4, 4) から内容が始まる(元画像 (8, 6) が新座標 (4, 4))。
    assert_eq!(*out.get_pixel(0, 0), Rgba(WHITE));
    assert_eq!(*out.get_pixel(4, 4), Rgba(BLACK));

    // padding が余白より大きければ画像全体になる(クランプ)。
    let full = run(&img, r#"{"operations":[{"op":"trim","padding":100}]}"#);
    assert_eq!(full.dimensions(), (40, 30));
}

/// `background` を明示すると、四隅の色ではなくその色が背景になる。
///
/// 四隅が黒・中央が白の画像で `background: "#000000"` を指定すると白い矩形が残る。
#[test]
fn trim_uses_the_explicit_background_color() {
    let img = canvas_with_box(20, 20, BLACK, WHITE, (5, 5, 6, 4));
    let out = run(
        &img,
        r###"{"operations":[{"op":"trim","background":"#000000"}]}"###,
    );
    assert_eq!(out.dimensions(), (6, 4));
    assert_eq!(*out.get_pixel(0, 0), Rgba(WHITE));
}

/// 背景を省略すると四隅の多数決になる。3 隅が白・1 隅だけ黒なら背景は白。
#[test]
fn trim_votes_on_the_four_corners_when_background_is_omitted() {
    let mut img = canvas_with_box(20, 20, WHITE, BLACK, (5, 5, 6, 4));
    // 右下隅だけ黒にする(3 対 1 で白が勝つ)。
    img.put_pixel(19, 19, Rgba(BLACK));
    let out = run(&img, r#"{"operations":[{"op":"trim"}]}"#);
    // 内容(5,5)-(10,8) と右下隅(19,19)の両方を含む外接矩形。
    assert_eq!(out.dimensions(), (15, 15));
}

/// 全画素が背景なら恒等 + 英語の警告(空画像を作らない)。
#[test]
fn trim_on_a_uniform_image_is_identity_with_a_warning() {
    let img = RgbaImage::from_pixel(12, 9, Rgba(WHITE));
    let out = apply(&encode_png(&img), r#"{"operations":[{"op":"trim"}]}"#);
    assert_eq!((out.width, out.height), (12, 9));
    assert!(
        out.warnings
            .iter()
            .any(|w| w == "operations[0] (trim): no content found, image left unchanged"),
        "expected a no-content warning, got {:?}",
        out.warnings
    );
}

/// アルファ画像: 完全透明の余白は RGB に関わらず背景と見なされる。
#[test]
fn trim_treats_transparent_margin_as_background_whatever_its_rgb_is() {
    // 透明部分に「ゴミ色」を入れておく(デコーダ次第で残りうる状況の模擬)。
    let img = canvas_with_box(24, 24, [200, 30, 90, 0], [0, 0, 0, 255], (6, 8, 5, 7));
    let out = run(&img, r#"{"operations":[{"op":"trim"}]}"#);
    assert_eq!(out.dimensions(), (5, 7));
}

/// tolerance は RGBA u8 の Chebyshev 距離。差 8 の縁は tolerance 8 で背景、7 では内容。
#[test]
fn trim_tolerance_is_the_chebyshev_distance_on_the_u8_grid() {
    let near_white = [247, 255, 255, 255];
    let img = canvas_with_box(16, 16, WHITE, near_white, (3, 3, 4, 4));
    let inside = run(&img, r#"{"operations":[{"op":"trim","tolerance":8}]}"#);
    assert_eq!(
        inside.dimensions(),
        (16, 16),
        "distance 8 <= tolerance 8 is background, so nothing is trimmed"
    );
    let outside = run(&img, r#"{"operations":[{"op":"trim","tolerance":7}]}"#);
    assert_eq!(outside.dimensions(), (4, 4));
}

/// trim は `crop.rect` と同じ平行移動を座標追跡へ積む。
/// したがって trim の**後**に置いた `coordinate_space: "source"` の crop は、
/// 元画像の座標で書いたとおりの領域を切り出す。
#[test]
fn source_space_crop_after_trim_maps_through_the_trim() {
    // 40x30、内容 (8, 6)-(23, 17)。内容の中に目印を置いて、切り出し位置を確かめる。
    let mut img = canvas_with_box(40, 30, WHITE, BLACK, (8, 6, 16, 12));
    img.put_pixel(12, 10, Rgba([255, 0, 0, 255]));

    let out = run(
        &img,
        r#"{"operations":[
            {"op":"trim"},
            {"op":"crop","rect":{"x":12,"y":10,"width":2,"height":2},
             "coordinate_space":"source"}
        ]}"#,
    );
    assert_eq!(out.dimensions(), (2, 2));
    // source 座標 (12, 10) の赤い目印が、切り出し結果の左上に来る。
    assert_eq!(*out.get_pixel(0, 0), Rgba([255, 0, 0, 255]));
    assert_eq!(*out.get_pixel(1, 1), Rgba(BLACK));
}

/// 決定論: 同じ入力 + 同じレシピを 2 回適用するとバイト同一。
#[test]
fn trim_is_deterministic() {
    let img = canvas_with_box(40, 30, WHITE, BLACK, (8, 6, 16, 12));
    let png = encode_png(&img);
    let json = r#"{"operations":[{"op":"trim","padding":3},{"op":"encode","format":"png"}]}"#;
    assert_eq!(apply(&png, json).bytes, apply(&png, json).bytes);
}

// ---------------------------------------------------------------------------
// threshold
// ---------------------------------------------------------------------------

/// 水平グレーランプ(BT.709 輝度 = v/255 と厳密一致するので期待値の手計算が単純)。
fn gray_ramp(values: &[u8]) -> RgbaImage {
    let mut img = RgbaImage::new(values.len() as u32, 1);
    for (x, &v) in values.iter().enumerate() {
        img.put_pixel(x as u32, 0, Rgba([v, v, v, 255]));
    }
    img
}

/// 出力の RGB は 0 か 255 しか取らない。
#[test]
fn threshold_output_is_only_black_or_white() {
    let img = gray_ramp(&(0..=255u8).collect::<Vec<_>>());
    for json in [
        r#"{"operations":[{"op":"threshold"}]}"#,
        r#"{"operations":[{"op":"threshold","method":"fixed","value":100}]}"#,
        r#"{"operations":[{"op":"threshold","method":"sauvola","window":15}]}"#,
    ] {
        let out = run(&img, json);
        for px in out.pixels() {
            for c in 0..3 {
                assert!(px[c] == 0 || px[c] == 255, "{json}: got {px:?}");
            }
        }
    }
}

/// アルファは 1 ビットも変わらない。
#[test]
fn threshold_preserves_alpha() {
    let mut img = RgbaImage::new(8, 1);
    for x in 0..8u32 {
        img.put_pixel(x, 0, Rgba([(x as u8) * 32, 40, 200, (x as u8) * 30]));
    }
    let out = run(
        &img,
        r#"{"operations":[{"op":"threshold","method":"otsu"}]}"#,
    );
    for x in 0..8u32 {
        assert_eq!(out.get_pixel(x, 0)[3], img.get_pixel(x, 0)[3], "at x={x}");
    }
}

/// fixed の境界: `luma > value` が白なので、`value` ちょうどの画素は黒側。
#[test]
fn fixed_threshold_puts_the_exact_value_on_the_black_side() {
    let img = gray_ramp(&[127, 128, 129]);
    let out = run(
        &img,
        r#"{"operations":[{"op":"threshold","method":"fixed","value":128}]}"#,
    );
    assert_eq!(out.get_pixel(0, 0)[0], 0, "luma 127 < 128 -> black");
    assert_eq!(out.get_pixel(1, 0)[0], 0, "luma 128 == 128 -> black");
    assert_eq!(out.get_pixel(2, 0)[0], 255, "luma 129 > 128 -> white");
}

/// otsu は二峰性の画像で 2 つの山の**間**(谷)を選ぶ。
///
/// 輝度 40 の画素と 200 の画素を半々に置いた画像で、閾値が 40..200 に入り、
/// 暗い側が黒・明るい側が白へ分かれることを固定する。
#[test]
fn otsu_separates_a_bimodal_image_at_the_valley() {
    let mut values = vec![40u8; 64];
    values.extend(vec![200u8; 64]);
    let img = gray_ramp(&values);
    let out = run(
        &img,
        r#"{"operations":[{"op":"threshold","method":"otsu"}]}"#,
    );
    for x in 0..64u32 {
        assert_eq!(out.get_pixel(x, 0)[0], 0, "dark mode at x={x}");
    }
    for x in 64..128u32 {
        assert_eq!(out.get_pixel(x, 0)[0], 255, "bright mode at x={x}");
    }
}

/// invert の対称性: invert の前後で全画素の RGB が反転する(XOR が 255)。
#[test]
fn invert_flips_every_pixel() {
    let img = gray_ramp(&(0..=255u8).collect::<Vec<_>>());
    for method in [
        r#""method":"otsu""#,
        r#""method":"fixed","value":90"#,
        r#""method":"sauvola","window":9"#,
    ] {
        let plain = run(
            &img,
            &format!(r#"{{"operations":[{{"op":"threshold",{method}}}]}}"#),
        );
        let flipped = run(
            &img,
            &format!(r#"{{"operations":[{{"op":"threshold",{method},"invert":true}}]}}"#),
        );
        for (a, b) in plain.pixels().zip(flipped.pixels()) {
            for c in 0..3 {
                assert_eq!(a[c] ^ b[c], 255, "{method}: {a:?} vs {b:?}");
            }
        }
    }
}

/// sauvola は画像より大きな窓でも端でパニックしない(窓を画像内へクランプする)。
#[test]
fn sauvola_clamps_its_window_at_the_image_edges() {
    let img = gray_ramp(&[10, 90, 200, 30, 250]);
    let out = run(
        &img,
        r#"{"operations":[{"op":"threshold","method":"sauvola","window":255,"k":0.5}]}"#,
    );
    assert_eq!(out.dimensions(), (5, 1));
}

/// 決定論: 同じ入力 + 同じレシピを 2 回適用するとバイト同一(3 method すべて)。
#[test]
fn threshold_is_deterministic() {
    let img = canvas_with_box(
        32,
        24,
        [220, 220, 210, 255],
        [40, 40, 45, 255],
        (4, 5, 20, 9),
    );
    let png = encode_png(&img);
    for method in [
        r#""method":"otsu""#,
        r#""method":"fixed","value":128"#,
        r#""method":"sauvola","window":11,"k":0.3"#,
    ] {
        let json = format!(
            r#"{{"operations":[{{"op":"threshold",{method}}},{{"op":"encode","format":"png"}}]}}"#
        );
        assert_eq!(
            apply(&png, &json).bytes,
            apply(&png, &json).bytes,
            "{method}"
        );
    }
}

/// mask 付き threshold は、他のトーン系 op と同じ合成経路に乗る
/// (白い側だけが 2 値化され、黒い側は元の画素のまま残る)。
#[test]
fn threshold_honours_a_mask_like_the_other_tone_ops() {
    // マスク: 左半分が白(適用)、右半分が黒(非適用)。
    let mut mask = RgbaImage::new(8, 1);
    for x in 0..8u32 {
        let v = if x < 4 { 255 } else { 0 };
        mask.put_pixel(x, 0, Rgba([v, v, v, 255]));
    }
    let mask_png = encode_png(&mask);

    struct OneAsset(Vec<u8>);
    impl atx_core::engine::AssetResolver for OneAsset {
        fn read_revision(&self, _revision_id: &str) -> atx_core::Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    let img = gray_ramp(&[10, 30, 200, 220, 10, 30, 200, 220]);
    let json = r#"{"operations":[{"op":"threshold","method":"fixed","value":128,
        "mask":{"revision_id":"rev_mask"}},{"op":"encode","format":"png"}]}"#;
    let out = atx_core::engine::apply_recipe_with_assets(
        &encode_png(&img),
        &recipe(json),
        &Limits::default(),
        &OneAsset(mask_png),
    )
    .expect("masked threshold should succeed");
    let out = decode_rgba(&out.bytes);

    // 左半分(マスク白)は 2 値化されている。
    assert_eq!(out.get_pixel(0, 0)[0], 0);
    assert_eq!(out.get_pixel(3, 0)[0], 255);
    // 右半分(マスク黒)は元の値のまま。
    assert_eq!(out.get_pixel(4, 0)[0], 10);
    assert_eq!(out.get_pixel(7, 0)[0], 220);
}

// ---------------------------------------------------------------------------
// validate(位置付きの英語メッセージ)
// ---------------------------------------------------------------------------

fn validate_err(json: &str) -> String {
    let r: TransformRecipe = serde_json::from_str(json).expect("recipe should parse");
    atx_core::recipe::validate(&r)
        .expect_err("recipe must be rejected")
        .to_string()
}

#[test]
fn threshold_validate_reports_the_operation_index_and_the_reason() {
    let cases: [(&str, &str); 5] = [
        (
            r#"{"operations":[{"op":"adjust"},{"op":"threshold","method":"sauvola","window":30}]}"#,
            "odd",
        ),
        (
            r#"{"operations":[{"op":"adjust"},{"op":"threshold","method":"fixed"}]}"#,
            "requires value",
        ),
        (
            r#"{"operations":[{"op":"adjust"},{"op":"threshold","method":"otsu","k":0.2}]}"#,
            "k is only valid",
        ),
        (
            r#"{"operations":[{"op":"adjust"},{"op":"threshold","method":"otsu","value":10}]}"#,
            "value is only valid",
        ),
        (
            r#"{"operations":[{"op":"adjust"},{"op":"threshold","method":"sauvola","window":257}]}"#,
            "3..=255",
        ),
    ];
    for (json, needle) in cases {
        let err = validate_err(json);
        assert!(
            err.contains("operations[1] (threshold)"),
            "{json}: missing position, got {err}"
        );
        assert!(
            err.contains(needle),
            "{json}: expected {needle:?}, got {err}"
        );
    }
}

#[test]
fn trim_validate_reports_the_operation_index_and_the_reason() {
    let err = validate_err(r#"{"operations":[{"op":"trim","padding":4097}]}"#);
    assert!(err.contains("operations[0] (trim)"), "{err}");
    assert!(err.contains("0..=4096"), "{err}");

    let err = validate_err(r#"{"operations":[{"op":"trim","background":"white"}]}"#);
    assert!(err.contains("operations[0] (trim)"), "{err}");
    assert!(err.contains("hex"), "{err}");
}

// ---------------------------------------------------------------------------
// proptest(冪等性)
// ---------------------------------------------------------------------------

/// 内容と背景を持つ小さな合成画像。背景は一様、内容は矩形。
fn arb_boxed_image() -> impl Strategy<Value = RgbaImage> {
    (
        4u32..24,
        4u32..24,
        any::<u8>(),
        any::<u8>(),
        0u32..3,
        0u32..3,
    )
        .prop_map(|(w, h, bg, fg, ox, oy)| {
            let mut img = RgbaImage::from_pixel(w, h, Rgba([bg, bg, bg, 255]));
            // 背景と区別できる内容色にする(差が小さいと trim が何も切らず、
            // 冪等性の主張自体は成り立つが検証の意味が薄くなる)。
            // 背景と必ず 128 離れた内容色にする(差が tolerance 未満だと
            // trim が何も切らず、冪等性の検証としては意味が薄くなる)。
            let _ = fg;
            let fg = bg.wrapping_add(128);
            let x = ox.min(w - 1);
            let y = oy.min(h - 1);
            let rw = (w - x).min(3);
            let rh = (h - y).min(3);
            for yy in y..y + rh {
                for xx in x..x + rw {
                    img.put_pixel(xx, yy, Rgba([fg, fg, fg, 255]));
                }
            }
            img
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// padding = 0 の trim は冪等: 一度切った画像をもう一度切っても何も変わらない
    /// (切った結果の縁には必ず内容画素が乗っているため)。
    #[test]
    fn trim_is_idempotent(img in arb_boxed_image()) {
        let once = run(&img, r#"{"operations":[{"op":"trim"}]}"#);
        let twice = run(&once, r#"{"operations":[{"op":"trim"}]}"#);
        prop_assert_eq!(once.dimensions(), twice.dimensions());
        prop_assert_eq!(once.into_raw(), twice.into_raw());
    }

    /// otsu / fixed は冪等: 出力は 0/255 の 2 値なので、再適用しても同じ側に落ちる。
    /// (sauvola は局所窓が 2 値画像上で変わるため保証しない = テストもしない。)
    #[test]
    fn otsu_and_fixed_thresholds_are_idempotent(img in arb_boxed_image()) {
        for json in [
            r#"{"operations":[{"op":"threshold","method":"otsu"}]}"#,
            r#"{"operations":[{"op":"threshold","method":"fixed","value":128}]}"#,
        ] {
            let once = run(&img, json);
            let twice = run(&once, json);
            prop_assert_eq!(once.into_raw(), twice.into_raw(), "{}", json);
        }
    }
}
