//! 入れ子 SVG(`<image href="data:image/svg+xml,...">`)の読み込み回数の上限
//! (2026-10-22 のレビューの回帰テスト)。
//!
//! usvg は `<image>` のインスタンスごとに入れ子 SVG を解析し直し(キャッシュしない)、
//! `<use>` で複製された分も別々に数える。要素数の上限(100 万)は木 1 本ごとにしか効かないので、
//! 数 KB の SVG で「解析回数 × 要素数」のメモリを確保させられた。import 時の検証でも同じ。

use std::collections::HashMap;

use atx_core::{
    apply_recipe_with_assets, validate_svg_asset, AssetResolver, AtxError, Limits, TransformRecipe,
};

struct MockAssets(HashMap<String, Vec<u8>>);

impl AssetResolver for MockAssets {
    fn read_revision(&self, revision_id: &str) -> atx_core::Result<Vec<u8>> {
        self.0
            .get(revision_id)
            .cloned()
            .ok_or_else(|| AtxError::InvalidRecipe(format!("missing {revision_id}")))
    }
}

fn canvas_png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([128, 128, 128, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// 1 px の緑の矩形 1 つを持つ入れ子 SVG を、`<use>` を `fan` 個ずつ 3 段重ねて
/// `fan^3` インスタンスにした SVG(フィクスチャは生成式。実在の図柄ではない)。
fn nested_svg(fan: u32) -> Vec<u8> {
    let inner = "data:image/svg+xml;utf8,&lt;svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'&gt;&lt;rect width='1' height='1' fill='%2300ff00'/&gt;&lt;/svg&gt;";
    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" width=\"{w}\" height=\"1\" viewBox=\"0 0 {w} 1\"><defs>",
        w = fan * fan * fan
    );
    s.push_str(&format!(
        "<g id=\"a\"><image x=\"0\" y=\"0\" width=\"1\" height=\"1\" href=\"{inner}\"/></g>"
    ));
    s.push_str("<g id=\"b\">");
    for i in 0..fan {
        s.push_str(&format!("<use xlink:href=\"#a\" x=\"{i}\"/>"));
    }
    s.push_str("</g><g id=\"c\">");
    for i in 0..fan {
        s.push_str(&format!("<use xlink:href=\"#b\" x=\"{}\"/>", i * fan));
    }
    s.push_str("</g></defs>");
    for i in 0..fan {
        s.push_str(&format!("<use xlink:href=\"#c\" x=\"{}\"/>", i * fan * fan));
    }
    s.push_str("</svg>");
    s.into_bytes()
}

fn overlay_wh(svg: Vec<u8>, width: u32, height: u32) -> Result<atx_core::EncodedOutput, AtxError> {
    let mut assets = HashMap::new();
    assets.insert("rev_svg".to_string(), svg);
    let recipe: TransformRecipe = serde_json::from_str(&format!(
        r#"{{"operations":[{{"op":"svg_overlay","svg_revision_id":"rev_svg","x":0,"y":0,"width":{width},"height":{height}}},{{"op":"encode","format":"png"}}]}}"#
    ))
    .unwrap();
    apply_recipe_with_assets(
        &canvas_png(width.max(2), height.max(2)),
        &recipe,
        &Limits::default(),
        &MockAssets(assets),
    )
}

/// 高さ 1 のラスタ(入れ子のテストは横 1 列に並ぶ)。
fn overlay(svg: Vec<u8>, width: u32) -> Result<atx_core::EncodedOutput, AtxError> {
    overlay_wh(svg, width, 1)
}

/// 1000 インスタンス(10 × 10 × 10)の入れ子は、import の検証で拒否され、
/// 描画では上限を超えた分が描かれずに警告になる。
#[test]
fn a_thousand_nested_svg_images_are_refused_at_import_and_capped_at_render() {
    let svg = nested_svg(10);
    let started = std::time::Instant::now();
    let reason = validate_svg_asset(&svg).expect_err("must be refused as an asset");
    assert!(reason.contains("nested"), "{reason}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));

    let out = overlay(svg, 1000).expect("render must still succeed");
    assert!(
        out.warnings.iter().any(|w| w.contains("nested")),
        "{:?}",
        out.warnings
    );
}

/// 少数(8 = 2 × 2 × 2)の入れ子は従来どおり検証を通り、警告なしで描かれる。
#[test]
fn a_few_nested_svg_images_still_validate_and_render() {
    let svg = nested_svg(2);
    assert_eq!(validate_svg_asset(&svg).unwrap(), Some((8, 1)));
    let out = overlay(svg, 8).unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let img = image::load_from_memory(&out.bytes).unwrap().to_rgba8();
    assert_eq!(img.get_pixel(0, 0).0, [0, 255, 0, 255]);
    assert_eq!(img.get_pixel(7, 0).0, [0, 255, 0, 255]);
    assert_eq!(img.get_pixel(0, 1).0, [128, 128, 128, 255]);
}

// ---------------------------------------------------------------------------
// resvg が描画時に確保するバッファの構造的な上限(2026-10-22 のレビュー)。
//
// resvg は opacity<1 / mask / clip-path / filter を持つグループ(分離グループ)ごとに
// 内容 bbox(filter では土台の 5 × 5 倍まで)の RGBA8 を確保し、祖先の層を保持したまま
// 子を描く。filter は primitive ごとの結果を全部保持し、feTurbulence は numOctaves に
// 比例した時間がかかる(デバッグビルドでは 31 以上で resvg 内部が加算あふれで panic)。
// pattern のタイルは枠で切り詰められず上限が無い。描画の前に usvg の木を歩いて粗い上限を掛ける。
// ---------------------------------------------------------------------------

fn wrap_svg(body: &str) -> Vec<u8> {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\" viewBox=\"0 0 64 64\">{body}</svg>"
    )
    .into_bytes()
}

fn nested_opacity_groups(depth: usize) -> Vec<u8> {
    let mut body = String::new();
    for _ in 0..depth {
        body.push_str("<g opacity=\"0.5\">");
    }
    body.push_str("<rect width=\"64\" height=\"64\" fill=\"#00ff00\"/>");
    for _ in 0..depth {
        body.push_str("</g>");
    }
    wrap_svg(&body)
}

/// 64 × 64 の SVG を等倍(64 × 64)で描く。
fn overlay_64(svg: Vec<u8>) -> Result<atx_core::EncodedOutput, AtxError> {
    overlay_wh(svg, 64, 64)
}

#[test]
fn isolated_groups_may_nest_8_deep_but_not_9() {
    assert!(overlay_64(nested_opacity_groups(8)).is_ok());
    let err = overlay_64(nested_opacity_groups(9))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("svg_overlay") && err.contains("nested"),
        "{err}"
    );
    // import の検証でも同じ上限が効く。
    assert!(validate_svg_asset(&nested_opacity_groups(8)).is_ok());
    assert!(validate_svg_asset(&nested_opacity_groups(9)).is_err());
}

#[test]
fn turbulence_octaves_are_capped() {
    let svg = |octaves: u32| {
        wrap_svg(&format!(
            "<filter id=\"f\"><feTurbulence baseFrequency=\"0.05\" numOctaves=\"{octaves}\"/></filter>\
             <rect width=\"64\" height=\"64\" filter=\"url(#f)\"/>"
        ))
    };
    assert!(overlay_64(svg(24)).is_ok());
    let err = overlay_64(svg(31)).unwrap_err().to_string();
    assert!(err.contains("numOctaves"), "{err}");
    assert!(validate_svg_asset(&svg(31)).is_err());
}

#[test]
fn a_filter_may_have_at_most_32_primitives() {
    let svg = |n: usize| {
        let prims: String = (0..n)
            .map(|_| "<feGaussianBlur stdDeviation=\"1\"/>")
            .collect();
        wrap_svg(&format!(
            "<filter id=\"f\">{prims}</filter><rect width=\"64\" height=\"64\" filter=\"url(#f)\"/>"
        ))
    };
    assert!(overlay_64(svg(32)).is_ok());
    let err = overlay_64(svg(33)).unwrap_err().to_string();
    assert!(err.contains("primitive"), "{err}");
}

#[test]
fn a_pattern_tile_larger_than_the_raster_limit_is_refused() {
    let svg = |size: u32| {
        wrap_svg(&format!(
            "<defs><pattern id=\"p\" patternUnits=\"userSpaceOnUse\" width=\"{size}\" height=\"{size}\">\
             <rect width=\"1\" height=\"1\" fill=\"#00ff00\"/></pattern></defs>\
             <rect width=\"64\" height=\"64\" fill=\"url(#p)\"/>"
        ))
    };
    assert!(overlay_64(svg(64)).is_ok());
    let err = overlay_64(svg(20_000)).unwrap_err().to_string();
    assert!(err.contains("pattern"), "{err}");
}
