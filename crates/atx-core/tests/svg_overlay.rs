//! `svg_overlay`(v0.8、DESIGN.md §9.9)のテスト。
//!
//! `ops::svg` は crate 非公開なので、レシピ → `apply_recipe_with_assets` →
//! 出力 PNG の画素という end-to-end の経路で検証する(serde・validate・
//! アセット解決・作業空間の切替・合成式・u8 往復まで込みで回帰させられる)。
//!
//! PNG 入出力を使うのは、sRGB 符号値空間の往復が u8 格子上でバイト同一であること
//! (`linear` のユニットテストが固定している硬いゲート)を利用して、
//! 「押した画素/押していない画素」を**厳密な等値**で言い切るため。

use std::collections::HashMap;

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe_with_assets, AssetResolver, Limits, Result};
use image::{Rgba, RgbaImage};

/// 40x20 の合成バッジ(青地 + 黄円)。テキストを含まない完全な自作フィクスチャ。
const BADGE: &[u8] = include_bytes!("../../../tests/fixtures/badge.svg");
const SCENE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");

const BADGE_ID: &str = "rev_badge";

// ---------------------------------------------------------------------------
// ヘルパ
// ---------------------------------------------------------------------------

struct MockAssets(HashMap<String, Vec<u8>>);

impl MockAssets {
    fn badge() -> Self {
        Self(HashMap::from([(BADGE_ID.to_string(), BADGE.to_vec())]))
    }

    fn with(id: &str, bytes: &[u8]) -> Self {
        Self(HashMap::from([(id.to_string(), bytes.to_vec())]))
    }
}

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

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// 一様な不透明キャンバス。
fn canvas(w: u32, h: u32, rgb: [u8; 3]) -> RgbaImage {
    let mut img = RgbaImage::new(w, h);
    for px in img.pixels_mut() {
        *px = Rgba([rgb[0], rgb[1], rgb[2], 255]);
    }
    img
}

/// レシピを 1 本適用して出力画像を返す(入出力とも PNG)。
fn run(src: &RgbaImage, json: &str, assets: &MockAssets) -> RgbaImage {
    let out = apply_recipe_with_assets(&encode_png(src), &recipe(json), &Limits::default(), assets)
        .expect("recipe should apply");
    decode_rgba(&out.bytes)
}

/// 警告付きで走らせる。
fn run_with_warnings(src: &RgbaImage, json: &str, assets: &MockAssets) -> (RgbaImage, Vec<String>) {
    let out = apply_recipe_with_assets(&encode_png(src), &recipe(json), &Limits::default(), assets)
        .expect("recipe should apply");
    (decode_rgba(&out.bytes), out.warnings)
}

/// バッジ 1 枚を貼るだけのレシピ JSON。
fn overlay_json(x: i64, y: i64, extra: &str) -> String {
    format!(
        r#"{{"operations":[{{"op":"svg_overlay","svg_revision_id":"{BADGE_ID}","x":{x},"y":{y}{extra}}}]}}"#
    )
}

// バッジの色(sRGB 符号値)。
const BLUE: [u8; 4] = [0, 0, 255, 255];
const YELLOW: [u8; 4] = [255, 255, 0, 255];
/// キャンバスのグレー。
const GRAY: [u8; 3] = [100, 100, 100];
const GRAY_PX: [u8; 4] = [100, 100, 100, 255];

// ---------------------------------------------------------------------------
// serde / canonical / validate
// ---------------------------------------------------------------------------

/// 正規化 JSON の形(atx-mcp との契約)。
/// `opacity` / `blend_mode` は既定でも必ず現れ、`width` / `height` は
/// 省略時に**現れない**(`skip_serializing_if`)。
#[test]
fn canonical_json_shape_is_pinned() {
    let r = recipe(&overlay_json(12, 34, ""));
    assert_eq!(
        atx_core::canonical_json(&r).unwrap(),
        r#"{"operations":[{"blend_mode":"normal","op":"svg_overlay","opacity":1.0,"svg_revision_id":"rev_badge","x":12,"y":34}]}"#
    );
    // 既定値を明示しても同じ canonical = 同じ recipe_hash。
    let explicit = recipe(&overlay_json(
        12,
        34,
        r#","opacity":1.0,"blend_mode":"normal""#,
    ));
    assert_eq!(
        atx_core::recipe_hash(&r).unwrap(),
        atx_core::recipe_hash(&explicit).unwrap()
    );
    // width/height を書くと canonical に現れる。
    let sized = recipe(&overlay_json(0, 0, r#","width":80,"height":40"#));
    assert_eq!(
        atx_core::canonical_json(&sized).unwrap(),
        r#"{"operations":[{"blend_mode":"normal","height":40,"op":"svg_overlay","opacity":1.0,"svg_revision_id":"rev_badge","width":80,"x":0,"y":0}]}"#
    );
}

/// v0.1 のピン留めハッシュは enum バリアント追加で動かない。
#[test]
fn existing_recipe_hash_is_unchanged_by_the_new_variant() {
    // v0.1 から据え置きのフラットレシピ(tests/layers.rs / tests/blend_ns.rs と同じピン)。
    let legacy = recipe(
        r#"{"operations":[
            {"op":"auto_orient"},
            {"op":"rotate","angle_degrees":-1.8,"crop":"largest_inscribed_rect"},
            {"op":"crop","aspect_ratio":"16:9","anchor":"center"},
            {"op":"resize","width":800,"fit":"cover"},
            {"op":"adjust","brightness":0.05,"contrast":0.02,"saturation":0.03,"sharpness":0.2},
            {"op":"encode","format":"jpeg","quality":85}
        ]}"#,
    );
    assert_eq!(
        atx_core::recipe_hash(&legacy).unwrap(),
        "884ea169e1027cf26d9140f6d2f7543904b2ca344667640f87820f528eaa175d",
        "adding svg_overlay must not move existing recipe hashes"
    );
}

/// 負の座標は許され、`x` / `y` は i64。
#[test]
fn negative_coordinates_round_trip() {
    let r = recipe(&overlay_json(-5, -7, ""));
    assert_eq!(
        atx_core::canonical_json(&r).unwrap(),
        r#"{"operations":[{"blend_mode":"normal","op":"svg_overlay","opacity":1.0,"svg_revision_id":"rev_badge","x":-5,"y":-7}]}"#
    );
}

#[test]
fn validate_matrix() {
    let bad = [
        // revision id
        (r#""svg_revision_id":"","x":0,"y":0"#, "must not be empty"),
        (
            r#""svg_revision_id":"badge","x":0,"y":0"#,
            "must start with",
        ),
        // opacity
        (
            r#""svg_revision_id":"rev_a","x":0,"y":0,"opacity":1.5"#,
            "opacity",
        ),
        (
            r#""svg_revision_id":"rev_a","x":0,"y":0,"opacity":-0.1"#,
            "opacity",
        ),
        // 寸法
        (
            r#""svg_revision_id":"rev_a","x":0,"y":0,"width":0"#,
            "width must be > 0",
        ),
        (
            r#""svg_revision_id":"rev_a","x":0,"y":0,"height":0"#,
            "height must be > 0",
        ),
        (
            r#""svg_revision_id":"rev_a","x":0,"y":0,"width":40000"#,
            "1..=32768",
        ),
        // 配置座標(回帰): 青天井の x / y は `apply` の加算を桁あふれさせていた。
        (
            r#""svg_revision_id":"rev_a","x":9223372036854775807,"y":0"#,
            "x must be within",
        ),
        (
            r#""svg_revision_id":"rev_a","x":0,"y":-9223372036854775808"#,
            "y must be within",
        ),
        (
            r#""svg_revision_id":"rev_a","x":100000001,"y":0"#,
            "x must be within",
        ),
    ];
    for (fields, needle) in bad {
        let json = format!(r#"{{"operations":[{{"op":"svg_overlay",{fields}}}]}}"#);
        let r: TransformRecipe = serde_json::from_str(&json).expect("should parse");
        let err = atx_core::recipe::validate(&r).unwrap_err().to_string();
        assert!(err.contains(needle), "{fields}: got {err}");
        assert!(err.contains("svg_overlay"), "{fields}: got {err}");
    }
    // 正常形。
    let ok = recipe(&overlay_json(
        0,
        0,
        r#","opacity":0.5,"blend_mode":"multiply""#,
    ));
    assert!(atx_core::recipe::validate(&ok).is_ok());
    // 上限いっぱいの大きな負オフセットは通る(全部クリップされるだけ)。
    let ok = recipe(&overlay_json(-100_000_000, -100_000_000, ""));
    assert!(atx_core::recipe::validate(&ok).is_ok());
}

/// 回帰: 上限いっぱいの負オフセットは panic せず、キャンバスを 1 画素も変えない。
#[test]
fn extreme_negative_placement_leaves_the_canvas_untouched() {
    let src = canvas(8, 8, GRAY);
    let out = run(
        &src,
        &overlay_json(-100_000_000, -100_000_000, ""),
        &MockAssets::badge(),
    );
    for px in out.pixels() {
        assert_eq!(px.0, GRAY_PX, "the overlay is entirely off-canvas");
    }
}

/// `svg_overlay` はマスクを取らない(`deny_unknown_fields` が弾く)。
#[test]
fn mask_is_rejected_as_an_unknown_field() {
    let json = format!(
        r#"{{"operations":[{{"op":"svg_overlay","svg_revision_id":"{BADGE_ID}","x":0,"y":0,
            "mask":{{"revision_id":"rev_m"}}}}]}}"#
    );
    let err = serde_json::from_str::<TransformRecipe>(&json).unwrap_err();
    assert!(err.to_string().contains("mask"), "{err}");
}

/// 必須フィールド欠落・未知のブレンドモードも serde が弾く。
#[test]
fn serde_rejects_missing_and_unknown() {
    assert!(serde_json::from_str::<TransformRecipe>(
        r#"{"operations":[{"op":"svg_overlay","x":0}]}"#
    )
    .is_err());
    let json = format!(
        r#"{{"operations":[{{"op":"svg_overlay","svg_revision_id":"{BADGE_ID}","x":0,"y":0,"blend_mode":"nope"}}]}}"#
    );
    assert!(serde_json::from_str::<TransformRecipe>(&json).is_err());
}

// ---------------------------------------------------------------------------
// 配置と画素
// ---------------------------------------------------------------------------

/// 押した画素は SVG の色そのもの、押していない画素は backdrop がビット単位で残る。
#[test]
fn stamped_and_unstamped_pixels_are_exact() {
    let src = canvas(80, 60, GRAY);
    let out = run(&src, &overlay_json(10, 5, ""), &MockAssets::badge());
    assert_eq!(out.dimensions(), (80, 60));

    // バッジは (10,5) から 40x20。円から離れた地の部分は純青。
    assert_eq!(
        out.get_pixel(40, 8).0,
        BLUE,
        "inside the badge, off the disc"
    );
    assert_eq!(out.get_pixel(45, 20).0, BLUE);
    // 円の中心 (10,10) + オフセット → 黄。
    assert_eq!(out.get_pixel(20, 15).0, YELLOW, "the disc centre");

    // バッジの外は backdrop がそのまま。
    assert_eq!(
        out.get_pixel(9, 5).0,
        GRAY_PX,
        "one pixel left of the badge"
    );
    assert_eq!(out.get_pixel(10, 4).0, GRAY_PX, "one pixel above the badge");
    assert_eq!(
        out.get_pixel(50, 5).0,
        GRAY_PX,
        "one pixel right of the badge"
    );
    assert_eq!(
        out.get_pixel(10, 25).0,
        GRAY_PX,
        "one pixel below the badge"
    );
    assert_eq!(out.get_pixel(0, 0).0, GRAY_PX);
    assert_eq!(out.get_pixel(79, 59).0, GRAY_PX);
}

/// バッジの外側が **1 画素も** 動いていないこと(全画素走査)。
#[test]
fn only_the_stamped_rect_changes() {
    let src = canvas(80, 60, GRAY);
    let out = run(&src, &overlay_json(10, 5, ""), &MockAssets::badge());
    for y in 0..60u32 {
        for x in 0..80u32 {
            let inside = (10..50).contains(&x) && (5..25).contains(&y);
            if !inside {
                assert_eq!(
                    out.get_pixel(x, y).0,
                    GRAY_PX,
                    "pixel ({x},{y}) outside the stamped rect must be untouched"
                );
            }
        }
    }
}

/// `width` だけ指定すると固有サイズ(40x20)の縦横比が保たれる = 80x40 になる。
#[test]
fn width_only_scales_preserving_the_aspect_ratio() {
    let src = canvas(120, 100, GRAY);
    let out = run(
        &src,
        &overlay_json(0, 0, r#","width":80"#),
        &MockAssets::badge(),
    );
    // 右下隅 (79, 39) はバッジ内、(80, 39) と (79, 40) は外。
    assert_eq!(out.get_pixel(79, 39).0, BLUE);
    assert_eq!(out.get_pixel(80, 39).0, GRAY_PX);
    assert_eq!(out.get_pixel(79, 40).0, GRAY_PX);

    // height だけの指定も同じ寸法に落ちる。
    let out2 = run(
        &src,
        &overlay_json(0, 0, r#","height":40"#),
        &MockAssets::badge(),
    );
    assert_eq!(out2.get_pixel(79, 39).0, BLUE);
    assert_eq!(out2.get_pixel(80, 39).0, GRAY_PX);

    // 両方指定は縦横比を無視する。
    let out3 = run(
        &src,
        &overlay_json(0, 0, r#","width":100,"height":10"#),
        &MockAssets::badge(),
    );
    assert_eq!(out3.get_pixel(99, 9).0, BLUE);
    assert_eq!(out3.get_pixel(99, 10).0, GRAY_PX);
}

/// 負の座標では、はみ出した部分だけがクリップされる(panic もエラーも起きない)。
#[test]
fn negative_placement_clips_instead_of_failing() {
    let src = canvas(80, 60, GRAY);
    // x = -15, y = -8 → 画像に残るのは バッジの (15..40, 8..20) の部分 = 25x12。
    // (0,0) はまだ黄円の内側なので、地の青は円から離れた点で見る。
    let out = run(&src, &overlay_json(-15, -8, ""), &MockAssets::badge());
    assert_eq!(
        out.get_pixel(15, 7).0,
        BLUE,
        "badge (30,15) landed at (15,7)"
    );
    assert_eq!(out.get_pixel(24, 11).0, BLUE, "last visible badge pixel");
    assert_eq!(out.get_pixel(25, 11).0, GRAY_PX, "one past the right edge");
    assert_eq!(out.get_pixel(24, 12).0, GRAY_PX, "one past the bottom edge");

    // 完全に画像の外なら恒等(全画素 backdrop のまま)。
    let outside = run(&src, &overlay_json(-100, -100, ""), &MockAssets::badge());
    for px in outside.pixels() {
        assert_eq!(px.0, GRAY_PX);
    }
}

// ---------------------------------------------------------------------------
// opacity / blend_mode(レイヤー合成と同じ式であることの確認)
// ---------------------------------------------------------------------------

/// opacity 0.5 は「backdrop と SVG のちょうど中間」。
///
/// 不透明どうしなので W3C の式は
/// `Co = (0.5 × Cs + 0.5 × Cb) / 1.0` に落ちる(テスト内で f64 で独立計算する)。
#[test]
fn opacity_half_blends_halfway() {
    let src = canvas(80, 60, GRAY);
    let out = run(
        &src,
        &overlay_json(10, 5, r#","opacity":0.5"#),
        &MockAssets::badge(),
    );

    let expect = |cs: u8, cb: u8| -> u8 {
        let v = 0.5 * (cs as f64 / 255.0) + 0.5 * (cb as f64 / 255.0);
        (v * 255.0).round() as u8
    };
    let px = out.get_pixel(40, 8).0;
    for ch in 0..3 {
        let want = expect(BLUE[ch], GRAY[ch]);
        assert!(
            px[ch].abs_diff(want) <= 1,
            "channel {ch}: got {} want {want} (pixel {px:?})",
            px[ch]
        );
    }
    assert_eq!(
        px[3], 255,
        "compositing onto an opaque backdrop stays opaque"
    );

    // opacity 0.0 は backdrop とバイト同一(端点は式ではなく分岐で確定している)。
    let none = run(
        &src,
        &overlay_json(10, 5, r#","opacity":0.0"#),
        &MockAssets::badge(),
    );
    for px in none.pixels() {
        assert_eq!(px.0, GRAY_PX);
    }
}

/// `multiply` は B(Cb, Cs) = Cb × Cs。純青を掛けると R/G が 0 になり、
/// B は backdrop の値がそのまま残る(× 1.0 は厳密な恒等)。
#[test]
fn multiply_blend_matches_the_w3c_formula() {
    let src = canvas(80, 60, GRAY);
    let out = run(
        &src,
        &overlay_json(10, 5, r#","blend_mode":"multiply""#),
        &MockAssets::badge(),
    );
    assert_eq!(
        out.get_pixel(40, 8).0,
        [0, 0, GRAY[2], 255],
        "multiply with pure blue zeroes R and G and keeps B exactly"
    );
    // 黄円のところは R/G が残り B が 0 になる(相補)。
    assert_eq!(out.get_pixel(20, 15).0, [GRAY[0], GRAY[1], 0, 255]);
}

// ---------------------------------------------------------------------------
// テキストと固有サイズ
// ---------------------------------------------------------------------------

/// `<text>` を含む SVG は警告を出し、**図形は描かれる**(テキストだけが落ちる)。
#[test]
fn text_svg_warns_but_still_renders_shapes() {
    let with_text =
        br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10" viewBox="0 0 20 10">
        <rect x="0" y="0" width="20" height="10" fill="#00ff00"/>
        <text x="2" y="8" font-size="8">ATX</text>
      </svg>"##;
    let assets = MockAssets::with("rev_text", with_text);
    let src = canvas(40, 30, GRAY);
    let json = r#"{"operations":[{"op":"svg_overlay","svg_revision_id":"rev_text","x":0,"y":0}]}"#;
    let (out, warnings) = run_with_warnings(&src, json, &assets);

    assert_eq!(
        warnings,
        vec![
            "operations[0] (svg_overlay): svg contains text elements; text is not rendered \
             (convert text to paths for deterministic output)"
                .to_string()
        ]
    );
    // 図形(緑の地)は描かれている。文字が描かれていたら黒画素が混ざるので、
    // 20x10 の全域が純緑であることがそのまま「テキストは落ちた」の証拠になる。
    for y in 0..10u32 {
        for x in 0..20u32 {
            assert_eq!(
                out.get_pixel(x, y).0,
                [0, 255, 0, 255],
                "({x},{y}) should be the plain green rect, with no glyph pixels"
            );
        }
    }
    // テキストの無いバッジでは警告が出ない。
    let (_, quiet) = run_with_warnings(&src, &overlay_json(0, 0, ""), &MockAssets::badge());
    assert!(quiet.is_empty(), "{quiet:?}");
}

/// 固有サイズを持たない SVG は、寸法が完全に指定されていなければ構造化エラー。
#[test]
fn missing_intrinsic_size_is_a_structured_error() {
    let no_size = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" height="100%"><rect width="5" height="5"/></svg>"#;
    let assets = MockAssets::with("rev_nosize", no_size);
    let src = canvas(40, 30, GRAY);
    let json =
        r#"{"operations":[{"op":"svg_overlay","svg_revision_id":"rev_nosize","x":0,"y":0}]}"#;
    let err = apply_recipe_with_assets(
        &encode_png(&src),
        &recipe(json),
        &Limits::default(),
        &assets,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("operation 0 (svg_overlay) failed"), "{err}");
    assert!(err.contains("no intrinsic size"), "{err}");
    assert!(err.contains("viewBox"), "{err}");

    // width と height を両方与えれば通る。
    let sized = r#"{"operations":[{"op":"svg_overlay","svg_revision_id":"rev_nosize","x":0,"y":0,"width":8,"height":8}]}"#;
    assert!(apply_recipe_with_assets(
        &encode_png(&src),
        &recipe(sized),
        &Limits::default(),
        &assets
    )
    .is_ok());
}

/// SVG でないアセットを指すと、op 番号と op 名を名指しする構造化エラーになる。
#[test]
fn non_svg_and_unknown_assets_name_the_operation() {
    let src = canvas(40, 30, GRAY);
    let json = r#"{"operations":[{"op":"svg_overlay","svg_revision_id":"rev_x","x":0,"y":0}]}"#;

    let not_svg = MockAssets::with("rev_x", b"LUT_3D_SIZE 2\n");
    let err = apply_recipe_with_assets(
        &encode_png(&src),
        &recipe(json),
        &Limits::default(),
        &not_svg,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("operation 0 (svg_overlay) failed"), "{err}");

    let missing = MockAssets::with("rev_other", BADGE);
    let err = apply_recipe_with_assets(
        &encode_png(&src),
        &recipe(json),
        &Limits::default(),
        &missing,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("operation 0 (svg_overlay) failed"), "{err}");
    assert!(err.contains("rev_x"), "{err}");
}

// ---------------------------------------------------------------------------
// 決定論とゴールデン
// ---------------------------------------------------------------------------

/// 同じ入力・同じレシピを 2 回走らせるとバイト同一(tiny-skia のラスタライズも決定論的)。
#[test]
fn two_runs_are_byte_identical() {
    let src = encode_png(&canvas(120, 90, GRAY));
    let r = recipe(&overlay_json(
        7,
        11,
        r#","width":83,"opacity":0.63,"blend_mode":"soft_light""#,
    ));
    let assets = MockAssets::badge();
    let a = apply_recipe_with_assets(&src, &r, &Limits::default(), &assets).unwrap();
    let b = apply_recipe_with_assets(&src, &r, &Limits::default(), &assets).unwrap();
    assert_eq!(sha256_hex(&a.bytes), sha256_hex(&b.bytes));
}

fn golden_recipe_json() -> String {
    format!(
        r#"{{"operations":[
            {{"op":"resize","width":320,"height":240,"fit":"cover"}},
            {{"op":"svg_overlay","svg_revision_id":"{BADGE_ID}","x":240,"y":210,
              "width":72,"opacity":0.8,"blend_mode":"screen"}},
            {{"op":"encode","format":"jpeg","quality":85}}
        ]}}"#
    )
}

/// ゴールデン: フィクスチャ → 320×240 → バッジを右下へ(幅 72 / opacity 0.8 / screen)
/// → jpeg85。出力バイト列の sha256 と `recipe_hash` を同時にピン留めする。
#[test]
fn golden_svg_overlay_pipeline_sha256() {
    let json = golden_recipe_json();
    let out = apply_recipe_with_assets(
        SCENE,
        &recipe(&json),
        &Limits::default(),
        &MockAssets::badge(),
    )
    .unwrap();
    assert_eq!((out.width, out.height), (320, 240));
    assert_eq!(out.mime_type, "image/jpeg");
    assert_eq!(
        sha256_hex(&out.bytes),
        "c1f67bdf7a5b69b411b66221bad5a109dcd91748252b2ca3cc3059d18349d1f3",
        "svg_overlay golden moved"
    );
    assert_eq!(
        atx_core::recipe_hash(&recipe(&json)).unwrap(),
        "08cf5c9d53e482d10a7fe7c1d32ce9112ec6f10df25183c1186e4b4b519aa606"
    );
}
