//! `clone` / `heal`(v0.7、DESIGN.md §9.8)のテスト。
//!
//! `ops::clone_heal` は crate 非公開なので、レシピ → `apply_recipe` → 出力 PNG の
//! 画素という end-to-end の経路で検証する(serde・validate・作業空間の切替・
//! u8 往復まで込みで回帰させられる)。
//!
//! PNG 入出力を使うのは、線形光の往復が u8 格子上でバイト同一であること
//! (`linear` のユニットテストが固定している硬いゲート)を利用して
//! **「複写は厳密である」**を画素の等値で言い切るため。

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe, Limits};
use image::{Rgba, RgbaImage};

const SCENE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");

// ---------------------------------------------------------------------------
// ヘルパ
// ---------------------------------------------------------------------------

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

fn build(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> RgbaImage {
    let mut img = RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            img.put_pixel(x, y, Rgba(f(x, y)));
        }
    }
    img
}

/// レシピを 1 本適用して出力画像を返す(入出力とも PNG)。
fn run(src: &RgbaImage, json: &str) -> RgbaImage {
    let out = apply_recipe(&encode_png(src), &recipe(json), &Limits::default())
        .expect("recipe should apply");
    decode_rgba(&out.bytes)
}

fn clone_json(src: (u32, u32), dest: (u32, u32), radius: u32, feather: f64) -> String {
    format!(
        r#"{{"operations":[{{"op":"clone","src_x":{},"src_y":{},"dest_x":{},"dest_y":{},
            "radius":{radius},"feather_px":{feather}}}]}}"#,
        src.0, src.1, dest.0, dest.1
    )
}

fn heal_json(src: (u32, u32), dest: (u32, u32), radius: u32, feather: f64) -> String {
    format!(
        r#"{{"operations":[{{"op":"heal","src_x":{},"src_y":{},"dest_x":{},"dest_y":{},
            "radius":{radius},"feather_px":{feather}}}]}}"#,
        src.0, src.1, dest.0, dest.1
    )
}

/// 中心 `(cx, cy)` からのユークリッド距離。
fn dist(x: u32, y: u32, cx: u32, cy: u32) -> f64 {
    let dx = x as f64 - cx as f64;
    let dy = y as f64 - cy as f64;
    (dx * dx + dy * dy).sqrt()
}

/// テクスチャ + 水平グラデーションの合成画像(heal の題材)。
///
/// - 低周波: `x` に対する線形ランプ(60 → 180)
/// - 高周波: 1px 市松 ±`CHECKER`(ガウスぼかしでほぼ完全に消える帯域)
const CHECKER: i32 = 6;

fn textured_gradient(x: u32, y: u32) -> i32 {
    let base = 60.0 + (x as f64) * 120.0 / 159.0;
    let checker = if (x + y).is_multiple_of(2) {
        CHECKER
    } else {
        -CHECKER
    };
    base.round() as i32 + checker
}

/// 高周波エネルギーの指標: 画素と上下左右の平均との差。
///
/// 1px 市松 ±A が線形ランプに乗っている場合、4 近傍の平均はランプの値そのもの
/// (線形なので中心の値)になるため `d = 2A` となり、低周波成分に一切依存しない。
fn high_freq(img: &RgbaImage, x: u32, y: u32) -> f64 {
    let at = |x: u32, y: u32| img.get_pixel(x, y).0[0] as f64;
    let neighbors = at(x - 1, y) + at(x + 1, y) + at(x, y - 1) + at(x, y + 1);
    at(x, y) - neighbors / 4.0
}

fn high_freq_variance(img: &RgbaImage, cx: u32, cy: u32, radius: f64) -> f64 {
    let mut vals = Vec::new();
    let r = radius.ceil() as i64;
    for dy in -r..=r {
        for dx in -r..=r {
            let (x, y) = ((cx as i64 + dx) as u32, (cy as i64 + dy) as u32);
            if dist(x, y, cx, cy) <= radius {
                vals.push(high_freq(img, x, y));
            }
        }
    }
    let mean = vals.iter().sum::<f64>() / vals.len() as f64;
    vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / vals.len() as f64
}

/// 円内の平均値(R チャンネル)。
fn disc_mean(img: &RgbaImage, cx: u32, cy: u32, radius: f64) -> f64 {
    let mut sum = 0.0;
    let mut n = 0.0;
    let r = radius.ceil() as i64;
    for dy in -r..=r {
        for dx in -r..=r {
            let (x, y) = ((cx as i64 + dx) as u32, (cy as i64 + dy) as u32);
            if dist(x, y, cx, cy) <= radius {
                sum += img.get_pixel(x, y).0[0] as f64;
                n += 1.0;
            }
        }
    }
    sum / n
}

// ---------------------------------------------------------------------------
// clone
// ---------------------------------------------------------------------------

/// 一様なソース領域を離れた場所へクローンすると、円の内側は**厳密な複写**、
/// 円の外側は 1 バイトも動かない。
#[test]
fn clone_copies_the_circle_exactly_and_leaves_the_rest_untouched() {
    // 左上に単色ブロック、それ以外はグラデーション。
    let src = build(64, 64, |x, y| {
        if x < 16 && y < 16 {
            [220, 40, 90, 255]
        } else {
            [(x * 3) as u8, (y * 3) as u8, 128, 255]
        }
    });
    let out = run(&src, &clone_json((8, 8), (44, 44), 6, 0.0));

    for y in 0..64u32 {
        for x in 0..64u32 {
            let d = dist(x, y, 44, 44);
            let got = out.get_pixel(x, y).0;
            if d <= 6.0 {
                // 円の内側: src 中心から同じオフセットの画素そのもの。
                let sx = (x as i64 + 8 - 44) as u32;
                let sy = (y as i64 + 8 - 44) as u32;
                assert_eq!(
                    got,
                    src.get_pixel(sx, sy).0,
                    "({x}, {y}) inside the circle must be an exact copy of ({sx}, {sy})"
                );
            } else {
                assert_eq!(
                    got,
                    src.get_pixel(x, y).0,
                    "({x}, {y}) is outside the circle and must be untouched"
                );
            }
        }
    }
}

/// `feather_px` は縁に単調な遷移帯を作る。内側(radius − feather)までは厳密な複写、
/// 縁の外側は完全に元のまま、その間は両者の中間になる。
#[test]
fn clone_feather_makes_a_monotonic_rim_band() {
    let src = build(64, 64, |x, _| {
        if x < 32 {
            [240, 240, 240, 255]
        } else {
            [20, 20, 20, 255]
        }
    });
    // 明るい側(8, 32)を暗い側(48, 32)へ、半径 10・フェザ 4 で複写。
    let out = run(&src, &clone_json((8, 32), (48, 32), 10, 4.0));

    // 中心から右へ 1px ずつ見ると、複写値(240)から元の値(20)へ単調に落ちる。
    let mut prev = 255i32;
    for dx in 0..=12u32 {
        let v = out.get_pixel(48 + dx, 32).0[0] as i32;
        assert!(v <= prev, "the rim band must be monotonic (dx = {dx})");
        prev = v;
    }
    assert_eq!(out.get_pixel(48, 32).0, [240, 240, 240, 255]); // 内側 = 厳密な複写
    assert_eq!(out.get_pixel(48 + 6, 32).0, [240, 240, 240, 255]); // r = 6 <= inner = 6
    assert_eq!(out.get_pixel(48 + 11, 32).0, [20, 20, 20, 255]); // 円の外 = 不変
    let mid = out.get_pixel(48 + 8, 32).0[0];
    assert!(mid > 20 && mid < 240, "feather band value {mid}");
}

/// **スナップショット意味論**: src と dest の円が重なっていても、読み出しは
/// 適用前の画素からしか行わない。半径の半分だけずらして自分自身へ複写しても
/// 「複写した画素をさらに複写する」尾引きが起きないこと。
///
/// 水平ランプを右へ 4px ずらして複写するので、逐次書き込みで読んでいたら
/// dest 列の値が「4px 前の書き込み結果」に汚染されて階段状になる
/// (この画像ならその差は数十階調になり、確実に検出できる)。
#[test]
fn overlapping_clone_reads_from_an_immutable_snapshot() {
    let src = build(64, 64, |x, _| [(x * 3) as u8, 100, 200, 255]);
    let out = run(&src, &clone_json((32, 32), (36, 32), 8, 0.0));

    for dy in -8i64..=8 {
        for dx in -8i64..=8 {
            if (dx * dx + dy * dy) as f64 > 64.0 {
                continue;
            }
            let (dx_u, dy_u) = ((36 + dx) as u32, (32 + dy) as u32);
            let (sx, sy) = ((32 + dx) as u32, (32 + dy) as u32);
            assert_eq!(
                out.get_pixel(dx_u, dy_u).0,
                src.get_pixel(sx, sy).0,
                "overlapping clone smeared at ({dx_u}, {dy_u})"
            );
        }
    }
}

/// 円が画像の外へはみ出しても中心が内側ならエラーにならず、はみ出した分は
/// 単に複写されない(クリップ)。
#[test]
fn clone_clips_circles_that_reach_past_the_edge() {
    let src = build(32, 32, |x, y| [(x * 8) as u8, (y * 8) as u8, 60, 255]);
    // dest 中心は右下隅、src 中心は左上隅。どちらの円も大きくはみ出す。
    let out = run(&src, &clone_json((1, 1), (30, 30), 6, 0.0));
    // 円内かつ src 側も画像内なら複写されている。
    assert_eq!(out.get_pixel(30, 30).0, src.get_pixel(1, 1).0);
    assert_eq!(out.get_pixel(31, 31).0, src.get_pixel(2, 2).0);
    // src 側が画像外(x < 0)になる画素は元のまま。
    assert_eq!(out.get_pixel(28, 30).0, src.get_pixel(28, 30).0);
}

// ---------------------------------------------------------------------------
// heal
// ---------------------------------------------------------------------------

/// **テクスチャ + トーン分解の品質**。
///
/// 題材: 水平グラデーション(低周波)に 1px 市松(高周波)を重ねた画像へ、
/// 暗いブレミッシュ(半径 3・−70 階調)を植える。クリーンな別領域から heal すると:
///
/// 1. **低周波が周囲のグラデーションへ戻る**: ブレミッシュ跡の平均が、
///    ブレミッシュを植えなかった参照画像の同じ位置の平均とほぼ一致する
///    (heal 前は 70 階調ずれている)
/// 2. **高周波は失われない**: 修復域の高周波分散が、ソース領域の高周波分散の
///    50% 以上を保つ(単なるぼかしなら 0 に落ちる)
#[test]
fn heal_restores_the_low_frequency_and_keeps_the_texture() {
    const BLEMISH: (u32, u32) = (40, 64);
    const SOURCE: (u32, u32) = (110, 64);
    const BLEMISH_R: f64 = 3.0;
    const HEAL_R: u32 = 30;

    let clean = build(160, 160, |x, y| {
        let v = textured_gradient(x, y).clamp(0, 255) as u8;
        [v, v, v, 255]
    });
    let blemished = build(160, 160, |x, y| {
        let mut v = textured_gradient(x, y);
        if dist(x, y, BLEMISH.0, BLEMISH.1) <= BLEMISH_R {
            v -= 70;
        }
        let v = v.clamp(0, 255) as u8;
        [v, v, v, 255]
    });

    let healed = run(&blemished, &heal_json(SOURCE, BLEMISH, HEAL_R, 8.0));

    // --- 1. 低周波 ---
    let want = disc_mean(&clean, BLEMISH.0, BLEMISH.1, BLEMISH_R);
    let before = disc_mean(&blemished, BLEMISH.0, BLEMISH.1, BLEMISH_R);
    let after = disc_mean(&healed, BLEMISH.0, BLEMISH.1, BLEMISH_R);
    assert!(
        (before - want).abs() > 60.0,
        "the fixture should start out clearly blemished (before {before}, want {want})"
    );
    assert!(
        (after - want).abs() < 10.0,
        "healed mean {after} should match the surrounding gradient {want} \
         (it was {before} before healing)"
    );

    // --- 2. 高周波 ---
    let src_var = high_freq_variance(&blemished, SOURCE.0, SOURCE.1, 15.0);
    let healed_var = high_freq_variance(&healed, BLEMISH.0, BLEMISH.1, 15.0);
    assert!(
        healed_var >= 0.5 * src_var,
        "healed high-frequency variance {healed_var} should keep at least 50% of the \
         source region's {src_var}"
    );

    // 参考: 単なるぼかしなら高周波はほぼ消えるので、この閾値は実質的な要求になっている。
    assert!(
        src_var > 100.0,
        "the fixture must actually be textured: {src_var}"
    );
}

/// 一様な領域どうしの heal は(detail = 0、tone = 元の値なので)ほぼ恒等。
#[test]
fn heal_between_uniform_regions_is_nearly_identity() {
    let src = build(48, 48, |_, _| [120, 130, 140, 255]);
    let out = run(&src, &heal_json((10, 10), (34, 34), 8, 0.0));
    for y in 0..48u32 {
        for x in 0..48u32 {
            let got = out.get_pixel(x, y).0;
            for c in 0..4 {
                assert!(
                    (got[c] as i32 - src.get_pixel(x, y).0[c] as i32).abs() <= 1,
                    "({x}, {y}) moved: {got:?}"
                );
            }
        }
    }
}

/// heal も円の外は 1 バイトも動かさない。
#[test]
fn heal_leaves_everything_outside_the_circle_untouched() {
    let src = build(64, 64, |x, y| [(x * 4) as u8, (y * 4) as u8, 90, 255]);
    let out = run(&src, &heal_json((16, 16), (44, 44), 8, 2.0));
    for y in 0..64u32 {
        for x in 0..64u32 {
            if dist(x, y, 44, 44) > 8.0 {
                assert_eq!(
                    out.get_pixel(x, y).0,
                    src.get_pixel(x, y).0,
                    "({x}, {y}) is outside the heal circle"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// エラー・validate・決定論
// ---------------------------------------------------------------------------

/// 中心が画像外なら **op 番号と op 名を名指しする**実行時エラー
/// (寸法は validate 時点では未知なので静的には弾けない)。
#[test]
fn center_outside_the_image_is_a_runtime_error_naming_the_op_index() {
    let src = build(32, 32, |_, _| [100, 100, 100, 255]);
    // 先頭に resize を置いて、エラーが operations[1] を名指しすることを確かめる。
    let json = r#"{"operations":[
        {"op":"resize","width":16,"height":16,"fit":"fill"},
        {"op":"clone","src_x":20,"src_y":2,"dest_x":4,"dest_y":4,"radius":3}
    ]}"#;
    let err = apply_recipe(&encode_png(&src), &recipe(json), &Limits::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("operation 1"), "{err}");
    assert!(err.contains("(clone)"), "{err}");
    assert!(err.contains("src center (20, 2)"), "{err}");
    assert!(
        err.contains("16x16"),
        "the error should name the current dimensions: {err}"
    );

    let json = r#"{"operations":[
        {"op":"heal","src_x":1,"src_y":1,"dest_x":1,"dest_y":40,"radius":3}
    ]}"#;
    let err = apply_recipe(&encode_png(&src), &recipe(json), &Limits::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("operation 0"), "{err}");
    assert!(err.contains("(heal)"), "{err}");
    assert!(err.contains("dest center (1, 40)"), "{err}");
}

/// 静的検証: radius は 1..=2048、feather_px は有限かつ 0..=200。
#[test]
fn validate_rejects_out_of_range_parameters() {
    let cases: &[(&str, &str)] = &[
        (
            r#"{"operations":[{"op":"clone","src_x":1,"src_y":1,"dest_x":2,"dest_y":2,"radius":0}]}"#,
            "radius must be within 1..=2048",
        ),
        (
            r#"{"operations":[{"op":"clone","src_x":1,"src_y":1,"dest_x":2,"dest_y":2,"radius":2049}]}"#,
            "radius must be within 1..=2048",
        ),
        (
            r#"{"operations":[{"op":"heal","src_x":1,"src_y":1,"dest_x":2,"dest_y":2,"radius":4,
                "feather_px":-1.0}]}"#,
            "feather_px must be within 0.0..=200",
        ),
        (
            r#"{"operations":[{"op":"heal","src_x":1,"src_y":1,"dest_x":2,"dest_y":2,"radius":4,
                "feather_px":200.5}]}"#,
            "feather_px must be within 0.0..=200",
        ),
    ];
    for (json, want) in cases {
        let err = atx_core::recipe::validate(&recipe(json))
            .expect_err("should be rejected")
            .to_string();
        assert!(err.contains(want), "{err}");
        assert!(err.starts_with("invalid recipe: operations[0]"), "{err}");
    }
}

/// 未知フィールドと必須フィールド欠落は serde が弾く(`deny_unknown_fields`)。
#[test]
fn serde_rejects_unknown_and_missing_fields() {
    let err = serde_json::from_str::<TransformRecipe>(
        r#"{"operations":[{"op":"clone","src_x":1,"src_y":1,"dest_x":2,"dest_y":2,
            "radius":3,"seed":7}]}"#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("seed"), "{err}");

    let err = serde_json::from_str::<TransformRecipe>(
        r#"{"operations":[{"op":"heal","src_x":1,"src_y":1,"dest_x":2,"dest_y":2}]}"#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("radius"), "{err}");
}

/// `feather_px` は省略可能で、既定 0.0 として正規化 JSON に現れる
/// (atx-mcp との契約: フィールドは常に出る = キー順の揺れが無い)。
#[test]
fn canonical_json_shape_is_stable() {
    let r = recipe(
        r#"{"operations":[{"op":"clone","src_x":10,"src_y":20,"dest_x":30,"dest_y":40,
            "radius":5}]}"#,
    );
    assert_eq!(
        atx_core::canonical_json(&r).unwrap(),
        "{\"operations\":[{\"dest_x\":30,\"dest_y\":40,\"feather_px\":0.0,\"op\":\"clone\",\
         \"radius\":5,\"src_x\":10,\"src_y\":20}]}"
    );
    // feather_px を明示しても同じ正規化 JSON = 同じ recipe_hash。
    let explicit = recipe(
        r#"{"operations":[{"op":"clone","src_x":10,"src_y":20,"dest_x":30,"dest_y":40,
            "radius":5,"feather_px":0.0}]}"#,
    );
    assert_eq!(
        atx_core::recipe_hash(&r).unwrap(),
        atx_core::recipe_hash(&explicit).unwrap()
    );
}

/// 2 回実行してバイト同一(決定論。乱数も反復探索も使っていないことの担保)。
#[test]
fn clone_and_heal_are_deterministic() {
    let src = build(96, 96, |x, y| {
        let v = textured_gradient(x % 160, y).clamp(0, 255) as u8;
        [v, v.wrapping_add(20), 200 - v / 2, 255]
    });
    let bytes = encode_png(&src);
    for json in [
        &clone_json((20, 20), (60, 60), 12, 3.0),
        &heal_json((20, 20), (60, 60), 12, 3.0),
    ] {
        let a = apply_recipe(&bytes, &recipe(json), &Limits::default()).unwrap();
        let b = apply_recipe(&bytes, &recipe(json), &Limits::default()).unwrap();
        assert_eq!(sha256_hex(&a.bytes), sha256_hex(&b.bytes));
    }
}

// ---------------------------------------------------------------------------
// ゴールデン
// ---------------------------------------------------------------------------

fn golden_recipe_json() -> &'static str {
    r#"{"operations":[
        {"op":"resize","width":320,"height":240,"fit":"cover"},
        {"op":"clone","src_x":80,"src_y":60,"dest_x":220,"dest_y":170,"radius":24,
         "feather_px":6.0},
        {"op":"heal","src_x":120,"src_y":200,"dest_x":60,"dest_y":110,"radius":18,
         "feather_px":4.0},
        {"op":"encode","format":"jpeg","quality":85}
    ]}"#
}

/// ゴールデン: フィクスチャ → 320×240 へ縮小 → clone(半径 24 / フェザ 6)→
/// heal(半径 18 / フェザ 4)→ jpeg85。
/// 出力バイト列の sha256 と `recipe_hash` を同時にピン留めする。
#[test]
fn golden_clone_heal_pipeline_sha256() {
    let out = apply_recipe(SCENE, &recipe(golden_recipe_json()), &Limits::default()).unwrap();
    assert_eq!((out.width, out.height), (320, 240));
    assert_eq!(out.mime_type, "image/jpeg");
    assert_eq!(
        sha256_hex(&out.bytes),
        "a063fc068d2407cbfd8f5b2795ac0fdefbba5fda4f3df82592e9f2e9cd864115",
        "clone/heal golden moved"
    );
    assert_eq!(
        atx_core::recipe_hash(&recipe(golden_recipe_json())).unwrap(),
        "d7c217f735ad6b9b88cb9a927bd14e4d452c87bab669ba9c2c689ff5c3d6f283"
    );
}
