//! 非 separable ブレンドモード(hue / saturation / color / luminosity)の
//! end-to-end テスト(v0.7、DESIGN.md §9.8)。
//!
//! `ops::blend` は crate 非公開なので、`apply_recipe_with_assets` +
//! モックの `AssetResolver` 経由で「レシピ → 合成 → エンコード」を丸ごと回す。
//! ブレンド関数そのものの**仕様値表**は `src/ops/blend.rs` のユニットテストにある
//! (このファイルはエンジン経路・serde・validate・意味論プローブを見る)。

use std::collections::HashMap;

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe_with_assets, AssetResolver, Limits, Result};
use image::{Rgba, RgbaImage};

const SCENE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");

// ---------------------------------------------------------------------------
// ヘルパ
// ---------------------------------------------------------------------------

struct MockAssets(HashMap<String, Vec<u8>>);

impl MockAssets {
    fn new(pairs: &[(&str, Vec<u8>)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
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

fn solid_png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut img = RgbaImage::new(w, h);
    for p in img.pixels_mut() {
        *p = Rgba(rgba);
    }
    encode_png(&img)
}

/// 「単色 backdrop(base 差し替え)へ単色レイヤーを mode で合成し、
/// 出力の 1 画素を読む」だけの最短経路。合成は sRGB 符号値・不透明どうしなので
/// 出力画素は `B(Cb, Cs)` を u8 へ丸めた値そのものになる。
fn blend_solid(mode: &str, backdrop: [u8; 3], source: [u8; 3]) -> [u8; 3] {
    const W: u32 = 8;
    const H: u32 = 8;
    let assets = MockAssets::new(&[
        (
            "rev_backdrop",
            solid_png(W, H, [backdrop[0], backdrop[1], backdrop[2], 255]),
        ),
        (
            "rev_source",
            solid_png(W, H, [source[0], source[1], source[2], 255]),
        ),
    ]);
    let json = format!(
        r#"{{"layers":[
             {{"source":{{"revision_id":"rev_backdrop"}}}},
             {{"source":{{"revision_id":"rev_source"}},"blend_mode":"{mode}"}}
           ],
           "operations":[{{"op":"encode","format":"png"}}]}}"#
    );
    // base は使わないが、入力バイト列としてフィクスチャを渡す(寸法は backdrop が決める)。
    let out = apply_recipe_with_assets(
        &solid_png(W, H, [0, 0, 0, 255]),
        &recipe(&json),
        &Limits::default(),
        &assets,
    )
    .expect("composite should succeed");
    let px = decode_rgba(&out.bytes).get_pixel(4, 4).0;
    [px[0], px[1], px[2]]
}

// ---------------------------------------------------------------------------
// 仕様の参照実装(テスト内で独立に書き下す)
// ---------------------------------------------------------------------------
//
// W3C compositing-1 の非 separable の定義を、実装とは別に**もう一度**書く。
// これが一致すれば「エンジン経路(sRGB 符号値でのデコード・合成・u8 丸め)」まで
// 込みで仕様どおりであることが言える。係数は仕様本文の 0.3 / 0.59 / 0.11。

fn ref_lum(c: [f64; 3]) -> f64 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}

fn ref_sat(c: [f64; 3]) -> f64 {
    c.iter().cloned().fold(f64::MIN, f64::max) - c.iter().cloned().fold(f64::MAX, f64::min)
}

fn ref_clip_color(mut c: [f64; 3]) -> [f64; 3] {
    let l = ref_lum(c);
    let n = c.iter().cloned().fold(f64::MAX, f64::min);
    let x = c.iter().cloned().fold(f64::MIN, f64::max);
    if n < 0.0 && (l - n).abs() > 1e-12 {
        for v in c.iter_mut() {
            *v = l + ((*v - l) * l) / (l - n);
        }
    }
    if x > 1.0 && (x - l).abs() > 1e-12 {
        for v in c.iter_mut() {
            *v = l + ((*v - l) * (1.0 - l)) / (x - l);
        }
    }
    c
}

fn ref_set_lum(c: [f64; 3], l: f64) -> [f64; 3] {
    let d = l - ref_lum(c);
    ref_clip_color([c[0] + d, c[1] + d, c[2] + d])
}

fn ref_set_sat(c: [f64; 3], s: f64) -> [f64; 3] {
    let mut idx = [0usize, 1, 2];
    idx.sort_by(|&a, &b| c[a].partial_cmp(&c[b]).unwrap().then(a.cmp(&b)));
    let (lo, mid, hi) = (idx[0], idx[1], idx[2]);
    let mut out = [0.0; 3];
    if c[hi] > c[lo] {
        out[mid] = ((c[mid] - c[lo]) * s) / (c[hi] - c[lo]);
        out[hi] = s;
    }
    out[lo] = 0.0;
    out
}

fn ref_blend(mode: &str, cb: [f64; 3], cs: [f64; 3]) -> [f64; 3] {
    match mode {
        "hue" => ref_set_lum(ref_set_sat(cs, ref_sat(cb)), ref_lum(cb)),
        "saturation" => ref_set_lum(ref_set_sat(cb, ref_sat(cs)), ref_lum(cb)),
        "color" => ref_set_lum(cs, ref_lum(cb)),
        "luminosity" => ref_set_lum(cb, ref_lum(cs)),
        other => panic!("unknown non-separable mode {other}"),
    }
}

fn to_unit(c: [u8; 3]) -> [f64; 3] {
    [
        c[0] as f64 / 255.0,
        c[1] as f64 / 255.0,
        c[2] as f64 / 255.0,
    ]
}

/// HSL 的なプローブ: 0..360 の色相(無彩色なら None)。
fn hue_degrees(c: [u8; 3]) -> Option<f64> {
    let [r, g, b] = to_unit(c);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if d <= 1e-9 {
        return None;
    }
    let h = if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    Some((h + 360.0) % 360.0)
}

fn hue_distance(a: f64, b: f64) -> f64 {
    let d = (a - b).abs() % 360.0;
    d.min(360.0 - d)
}

// ---------------------------------------------------------------------------
// 1. エンジン経路が仕様の参照実装と一致する
// ---------------------------------------------------------------------------

/// 4 モード × 複数の色対を、テスト内の独立な参照実装と ±1 で突き合わせる。
/// (合成は sRGB 符号値・不透明どうしなので、出力は `B(Cb, Cs)` の u8 丸めそのもの)
#[test]
fn engine_matches_an_independent_reference_implementation() {
    const PAIRS: &[([u8; 3], [u8; 3])] = &[
        ([80, 140, 200], [200, 90, 60]),  // 一般的な青 × 橙
        ([128, 128, 128], [255, 0, 0]),   // 無彩色 backdrop × 純赤(ClipColor 発動)
        ([200, 40, 90], [128, 128, 128]), // 有彩色 backdrop × 無彩色ソース(Sat = 0)
        ([10, 10, 10], [240, 200, 30]),   // ほぼ黒の backdrop
        ([250, 245, 240], [20, 60, 200]), // ほぼ白の backdrop(ClipColor 発動)
        ([90, 90, 200], [90, 200, 90]),   // 同値成分あり(タイブレーク経路)
    ];
    for mode in ["hue", "saturation", "color", "luminosity"] {
        for &(cb, cs) in PAIRS {
            let got = blend_solid(mode, cb, cs);
            let want = ref_blend(mode, to_unit(cb), to_unit(cs));
            for c in 0..3 {
                let want_u8 = (want[c].clamp(0.0, 1.0) * 255.0).round();
                let diff = (got[c] as f64 - want_u8).abs();
                assert!(
                    diff <= 1.0,
                    "{mode}: B({cb:?}, {cs:?})[{c}] = {} but the spec gives {want_u8}",
                    got[c]
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. 意味論プローブ(HSL 的な性質)
// ---------------------------------------------------------------------------

/// `color` は **ソースの色相 + 彩度**に **backdrop の輝度**を載せる。
#[test]
fn color_takes_hue_and_saturation_from_source_and_luminosity_from_backdrop() {
    let cb = [70u8, 120, 190]; // 青
    let cs = [210u8, 120, 40]; // 橙
    let out = blend_solid("color", cb, cs);

    // 輝度は backdrop 由来(ClipColor は輝度を保つので、クリップしても成立する)。
    let l_out = ref_lum(to_unit(out));
    let l_b = ref_lum(to_unit(cb));
    assert!(
        (l_out - l_b).abs() < 2.0 / 255.0,
        "color: Lum(out) = {l_out} should match Lum(backdrop) = {l_b} (out = {out:?})"
    );
    // 色相はソース由来。
    let (h_out, h_s, h_b) = (
        hue_degrees(out).unwrap(),
        hue_degrees(cs).unwrap(),
        hue_degrees(cb).unwrap(),
    );
    assert!(
        hue_distance(h_out, h_s) < 3.0,
        "color: hue(out) = {h_out} should match hue(source) = {h_s}"
    );
    assert!(
        hue_distance(h_out, h_b) > 60.0,
        "color: hue(out) = {h_out} must not stay on the backdrop hue {h_b}"
    );
}

/// `luminosity` は `color` の相補: **backdrop の色相**に**ソースの輝度**。
#[test]
fn luminosity_takes_luminance_from_source_and_hue_from_backdrop() {
    let cb = [70u8, 120, 190];
    let cs = [210u8, 120, 40];
    let out = blend_solid("luminosity", cb, cs);

    let l_out = ref_lum(to_unit(out));
    let l_s = ref_lum(to_unit(cs));
    assert!(
        (l_out - l_s).abs() < 2.0 / 255.0,
        "luminosity: Lum(out) = {l_out} should match Lum(source) = {l_s} (out = {out:?})"
    );
    let (h_out, h_b) = (hue_degrees(out).unwrap(), hue_degrees(cb).unwrap());
    assert!(
        hue_distance(h_out, h_b) < 3.0,
        "luminosity: hue(out) = {h_out} should stay on the backdrop hue {h_b}"
    );
}

/// `hue` は**ソースの色相**に **backdrop の彩度と輝度**を載せる。
#[test]
fn hue_takes_hue_from_source_and_saturation_luminance_from_backdrop() {
    // ClipColor が効かない穏やかな組み合わせを選ぶ(効くと彩度が落ちるため)。
    let cb = [110u8, 130, 150];
    let cs = [200u8, 80, 90];
    let out = blend_solid("hue", cb, cs);

    let l_out = ref_lum(to_unit(out));
    let l_b = ref_lum(to_unit(cb));
    assert!(
        (l_out - l_b).abs() < 2.0 / 255.0,
        "hue: Lum(out) = {l_out} should match Lum(backdrop) = {l_b} (out = {out:?})"
    );
    let s_out = ref_sat(to_unit(out));
    let s_b = ref_sat(to_unit(cb));
    assert!(
        (s_out - s_b).abs() < 3.0 / 255.0,
        "hue: Sat(out) = {s_out} should match Sat(backdrop) = {s_b} (out = {out:?})"
    );
    let (h_out, h_s) = (hue_degrees(out).unwrap(), hue_degrees(cs).unwrap());
    assert!(
        hue_distance(h_out, h_s) < 4.0,
        "hue: hue(out) = {h_out} should match hue(source) = {h_s}"
    );
}

/// `saturation` は**ソースの彩度**だけを移す(色相・輝度は backdrop のまま)。
#[test]
fn saturation_takes_saturation_from_source_only() {
    let cb = [120u8, 150, 90]; // Sat = 60/255
    let cs = [200u8, 40, 60]; // Sat = 160/255
    let out = blend_solid("saturation", cb, cs);

    let s_out = ref_sat(to_unit(out));
    let s_s = ref_sat(to_unit(cs));
    assert!(
        (s_out - s_s).abs() < 3.0 / 255.0,
        "saturation: Sat(out) = {s_out} should match Sat(source) = {s_s} (out = {out:?})"
    );
    let l_out = ref_lum(to_unit(out));
    let l_b = ref_lum(to_unit(cb));
    assert!(
        (l_out - l_b).abs() < 2.0 / 255.0,
        "saturation: Lum(out) = {l_out} should match Lum(backdrop) = {l_b}"
    );
    let (h_out, h_b) = (hue_degrees(out).unwrap(), hue_degrees(cb).unwrap());
    assert!(
        hue_distance(h_out, h_b) < 4.0,
        "saturation: hue(out) = {h_out} should stay on the backdrop hue {h_b}"
    );
}

/// **無彩色の端点(Sat = 0)**。
/// - `color` / `hue` に無彩色ソースを渡すと、出力は backdrop の輝度の無彩色
/// - `saturation` に無彩色ソースを渡すと、backdrop が脱色される
/// - 無彩色 backdrop に `hue` / `saturation` を掛けても無彩色のまま
#[test]
fn achromatic_endpoints() {
    let gray = [130u8, 130, 130];
    let colorful = [200u8, 60, 30];

    for mode in ["color", "hue"] {
        let out = blend_solid(mode, colorful, gray);
        assert_eq!(out[0], out[1], "{mode} with a gray source must stay gray");
        assert_eq!(out[1], out[2], "{mode} with a gray source must stay gray");
        let l_out = ref_lum(to_unit(out));
        let l_b = ref_lum(to_unit(colorful));
        assert!((l_out - l_b).abs() < 2.0 / 255.0, "{mode}: {out:?}");
    }

    let desaturated = blend_solid("saturation", colorful, gray);
    assert_eq!(desaturated[0], desaturated[1]);
    assert_eq!(desaturated[1], desaturated[2]);

    for mode in ["hue", "saturation"] {
        let out = blend_solid(mode, gray, colorful);
        // 無彩色 backdrop は Sat = 0 / Cmax == Cmin なのでどちらの経路でも無彩色のまま。
        assert_eq!(out[0], out[1], "{mode} over a gray backdrop: {out:?}");
        assert_eq!(out[1], out[2], "{mode} over a gray backdrop: {out:?}");
    }

    // luminosity に無彩色ソース = backdrop をそのグレーの輝度へ動かすだけ。
    let out = blend_solid("luminosity", colorful, gray);
    let l_out = ref_lum(to_unit(out));
    assert!(
        (l_out - ref_lum(to_unit(gray))).abs() < 2.0 / 255.0,
        "{out:?}"
    );
}

/// `Cs == Cb` はどのモードでも恒等(SetSat / SetLum の差分が 0)。
#[test]
fn identical_layers_are_the_identity_for_every_non_separable_mode() {
    let c = [90u8, 140, 60];
    for mode in ["hue", "saturation", "color", "luminosity"] {
        let out = blend_solid(mode, c, c);
        for ch in 0..3 {
            assert!(
                (out[ch] as i32 - c[ch] as i32).abs() <= 1,
                "{mode}: {out:?} should be {c:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 3. serde / validate(atx-mcp との契約)
// ---------------------------------------------------------------------------

/// 4 種が snake_case で往復し、正規化 JSON に素直に出る。
#[test]
fn non_separable_modes_round_trip_as_snake_case() {
    for mode in ["hue", "saturation", "color", "luminosity"] {
        let json = format!(
            r#"{{"layers":[{{"source":"base"}},{{"source":"base","blend_mode":"{mode}"}}],
                "operations":[{{"op":"encode","format":"png"}}]}}"#
        );
        let r = recipe(&json);
        let canonical = atx_core::canonical_json(&r).unwrap();
        assert!(
            canonical.contains(&format!("\"blend_mode\":\"{mode}\"")),
            "{canonical}"
        );
        // 往復。
        let back: TransformRecipe = serde_json::from_str(&canonical).unwrap();
        assert_eq!(back, r);
    }
}

/// 未知のモード名は拒否される(エラーが有効値を列挙するのは serde の既定挙動)。
#[test]
fn unknown_blend_mode_is_rejected() {
    let err = serde_json::from_str::<TransformRecipe>(
        r#"{"layers":[{"source":"base"},{"source":"base","blend_mode":"colour"}],
            "operations":[{"op":"encode","format":"png"}]}"#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("colour"), "{err}");
    assert!(
        err.contains("luminosity"),
        "expected variants listed: {err}"
    );
}

/// **backdrop レイヤーの規則は据え置き**: 先頭レイヤーは normal でなければならない。
/// 非 separable を追加してもこの制約は緩んでいない。
#[test]
fn backdrop_layer_still_rejects_non_separable_blend_modes() {
    for mode in ["hue", "saturation", "color", "luminosity"] {
        let json = format!(
            r#"{{"layers":[{{"source":"base","blend_mode":"{mode}"}}],
                "operations":[{{"op":"encode","format":"png"}}]}}"#
        );
        let err = atx_core::recipe::validate(&recipe(&json))
            .unwrap_err()
            .to_string();
        assert!(err.contains("layers[0] is the backdrop"), "{mode}: {err}");
        assert!(
            err.contains("blend_mode must be \"normal\""),
            "{mode}: {err}"
        );
    }
}

/// 既存の separable ゴールデンレシピのハッシュは enum 拡張で動かない
/// (バリアント**追加**は既存の値の serde 表現を変えないため)。
#[test]
fn pinned_recipe_hashes_are_unmoved_by_the_new_variants() {
    // v0.1 から据え置きのフラットレシピ(tests/layers.rs と同じピン)。
    let flat = recipe(
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
        atx_core::recipe_hash(&flat).unwrap(),
        "884ea169e1027cf26d9140f6d2f7543904b2ca344667640f87820f528eaa175d"
    );
    // v0.6 のレイヤー付きゴールデンレシピ(tests/layers.rs の 3 レイヤーと同じピン)。
    let layered = recipe(
        r#"{
          "layers":[
            {"source":"base"},
            {"source":{"revision_id":"rev_tex1"},
             "ops":[{"op":"blur","sigma":2.0}],
             "mask":{"revision_id":"rev_m1","feather_px":4.0},
             "blend_mode":"multiply",
             "opacity":0.5}
          ],
          "operations":[{"op":"encode","format":"png"}]
        }"#,
    );
    assert_eq!(
        atx_core::canonical_json(&layered).unwrap(),
        "{\"layers\":[{\"blend_mode\":\"normal\",\"opacity\":1.0,\"ops\":[],\"source\":\"base\"},\
         {\"blend_mode\":\"multiply\",\"mask\":{\"feather_px\":4.0,\"invert\":false,\
         \"revision_id\":\"rev_m1\"},\"opacity\":0.5,\"ops\":[{\"op\":\"blur\",\"sigma\":2.0}],\
         \"source\":{\"revision_id\":\"rev_tex1\"}}],\"operations\":[{\"format\":\"png\",\
         \"op\":\"encode\"}]}"
    );
}

// ---------------------------------------------------------------------------
// 4. 決定論 + ゴールデン
// ---------------------------------------------------------------------------

fn color_layer_recipe_json() -> String {
    r#"{
      "layers":[
        {"source":"base","ops":[{"op":"resize","width":320,"height":240,"fit":"cover"}]},
        {"source":{"revision_id":"rev_tint"},"blend_mode":"color","opacity":0.8},
        {"source":{"revision_id":"rev_tint"},"blend_mode":"luminosity","opacity":0.35}
      ],
      "operations":[{"op":"encode","format":"jpeg","quality":85}]
    }"#
    .to_string()
}

fn color_layer_assets() -> MockAssets {
    MockAssets::new(&[("rev_tint", solid_png(320, 240, [210, 120, 40, 255]))])
}

#[test]
fn non_separable_composite_is_deterministic() {
    let assets = color_layer_assets();
    let r = recipe(&color_layer_recipe_json());
    let a = apply_recipe_with_assets(SCENE, &r, &Limits::default(), &assets).unwrap();
    let b = apply_recipe_with_assets(SCENE, &r, &Limits::default(), &assets).unwrap();
    assert_eq!(sha256_hex(&a.bytes), sha256_hex(&b.bytes));
}

/// ゴールデン: フィクスチャを 320×240 に縮小 → 単色を `color` 0.8 で着色 →
/// 同じ単色を `luminosity` 0.35 で載せる → jpeg85。
/// 出力バイト列の sha256 と `recipe_hash` を同時にピン留めする
/// (どちらが動いてもこのテストが落ちる)。
#[test]
fn golden_non_separable_composite_sha256() {
    let assets = color_layer_assets();
    let json = color_layer_recipe_json();
    let out = apply_recipe_with_assets(SCENE, &recipe(&json), &Limits::default(), &assets).unwrap();
    assert_eq!((out.width, out.height), (320, 240));
    assert_eq!(out.mime_type, "image/jpeg");
    assert_eq!(
        sha256_hex(&out.bytes),
        "a09924fd624931c773012adc31de43437fb2230ab71c6e29870aec5bf90b121a",
        "non-separable composite golden moved"
    );
    assert_eq!(
        atx_core::recipe_hash(&recipe(&json)).unwrap(),
        "2d1b01ac591fb87684b44e0cc6669ff50087ba601d9440e74de19fece30850ef"
    );
}
