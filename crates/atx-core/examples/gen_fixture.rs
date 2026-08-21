//! テストフィクスチャ `tests/fixtures/synthetic_scene.jpg` および
//! `evals/fixtures/tilted_scene.jpg` の生成器。
//!
//! ```sh
//! cargo run -p atx-core --example gen_fixture
//! ```
//!
//! ## tilted_scene.jpg(evals/tasks/t01_straighten_eyecatch.json 用)
//!
//! `synthetic_scene.jpg` はほぼ水平(atx-geometry の傾き検出が ~0° を返す)なため、
//! 「まっすぐにして」という eval タスクで `rotate` op を使わなくても正しく振る舞える
//! 状態だった。これは eval タスク側の不備であり、フィクスチャを客観的に傾いた画像に
//! する方が正しい修正(docs/DESIGN.md 参照)。
//!
//! `synthetic_scene.jpg` と同じ合成シーンに対し、atx-core の決定論エンジン自身
//! (`apply_recipe`)で `Rotate { angle_degrees: -2.4, crop: largest_inscribed_rect }`
//! を適用したものを書き出す。atx-core の `Rotate.angle_degrees` は「正 = 時計回り」、
//! atx-geometry の `recommended_angle_degrees` も同じ規約(「正 = 時計回りに回すと
//! 水平になる」)なので、-2.4° 回転させた画像を水平に戻す補正角は理論上ちょうど
//! +2.4° になる。本生成器はこれを仮定で終わらせず、生成直後に
//! `atx_geometry::detect_tilt` を実際に走らせて `recommended_angle_degrees` が
//! +2.4° 近辺・十分な confidence であることを assert で検証する。
//!
//! ## 方針(DESIGN §9 フィクスチャ方針)
//!
//! リポジトリに第三者/個人の写真を置かないため、テスト用の「写真らしい」画像は
//! すべてこの生成器で合成する。乱数源は固定シードの LCG のみで、`rand` も時刻も
//! 使わない。したがって何度実行してもバイト同一の JPEG が得られる
//! (この例自身が 2 回エンコードして一致を検証する)。
//!
//! ## 描くもの(パイプライン全体を動かすための構造)
//!
//! - 空 → 地面の縦方向グラデーション(地平線に強い水平エッジ)
//! - 軸平行の「ビル」矩形 4 棟 + 窓格子(水平・垂直の支配的な直線群)
//!   → atx-geometry の傾き検出が H/V 両族を拾い、ほぼ 0° を返す
//! - 街灯のポール、地面のタイル目地(細かい直線)
//! - LCG 由来の微細ノイズと粒状テクスチャ(JPEG が平坦画像にならないように)
//! - RGB 各チャンネルに散らばる色(チャンネル取り違えを検出できるよう非対称にする)
//!
//! メタデータは APP2 ICC(ダミー 256 バイト)のみ。EXIF は付けない
//! (フィクスチャが EXIF レスであることをテストが前提にしている)。

use std::path::PathBuf;

use image::{Rgb, RgbImage};

use atx_core::{Limits, Operation, OutputFormat, RotateCrop, TransformRecipe};
use atx_geometry::{detect_tilt, DetectParams};

/// `tilted_scene.jpg` に加える回転角(度、atx-core の規約で正 = 時計回り)。
/// 水平に戻す補正角はこの符号反転(+2.4°)になる想定。
const TILT_ROTATE_DEGREES: f64 = -2.4;

/// フィクスチャの寸法(実写フィクスチャからの移行時に維持した値)。
const WIDTH: u32 = 1477;
const HEIGHT: u32 = 1108;
/// 地平線の y 座標。
const HORIZON: u32 = 664;
/// JPEG 品質(atx-core の既定と同じ)。
const QUALITY: u8 = 85;

/// 決定論的な擬似乱数(Numerical Recipes の LCG)。
struct Lcg(u32);

impl Lcg {
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0
    }

    /// -1.0 ..= 1.0 のノイズ。
    fn noise(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1 << 23) as f32 - 1.0
    }

    fn byte(&mut self) -> u8 {
        (self.next_u32() >> 24) as u8
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        lerp(a[0], b[0], t),
        lerp(a[1], b[1], t),
        lerp(a[2], b[2], t),
    ]
}

fn put(buf: &mut [[f32; 3]], x: u32, y: u32, c: [f32; 3]) {
    if x < WIDTH && y < HEIGHT {
        buf[(y * WIDTH + x) as usize] = c;
    }
}

/// 軸平行の塗り矩形(x, y は左上、範囲外はクリップ)。
fn fill_rect(buf: &mut [[f32; 3]], x0: u32, y0: u32, w: u32, h: u32, c: [f32; 3]) {
    for y in y0..(y0 + h).min(HEIGHT) {
        for x in x0..(x0 + w).min(WIDTH) {
            put(buf, x, y, c);
        }
    }
}

/// 建物 1 棟: 面ごとに明度差のあるファサード + 窓格子 + 屋上のパラペット。
///
/// 窓格子が水平・垂直エッジを大量に供給するので、傾き検出の H/V 両族が立つ。
#[allow(clippy::too_many_arguments)]
fn building(
    buf: &mut [[f32; 3]],
    x0: u32,
    top: u32,
    w: u32,
    base: [f32; 3],
    win: [f32; 3],
    win_w: u32,
    win_h: u32,
    rng: &mut Lcg,
) {
    let h = HORIZON.saturating_sub(top) + 24;

    // ファサード: 上ほどわずかに明るい(空からの照り返し)縦グラデーション。
    for y in top..(top + h).min(HEIGHT) {
        let t = (y - top) as f32 / h.max(1) as f32;
        let c = mix(
            [base[0] * 1.08, base[1] * 1.08, base[2] * 1.10],
            [base[0] * 0.82, base[1] * 0.82, base[2] * 0.86],
            t,
        );
        for x in x0..(x0 + w).min(WIDTH) {
            put(buf, x, y, c);
        }
    }

    // 屋上パラペット(強い水平エッジ)。
    fill_rect(
        buf,
        x0,
        top,
        w,
        9,
        [base[0] * 0.55, base[1] * 0.55, base[2] * 0.62],
    );
    // 両端の縁(強い垂直エッジ)。
    fill_rect(
        buf,
        x0,
        top,
        3,
        h,
        [base[0] * 0.60, base[1] * 0.60, base[2] * 0.66],
    );
    fill_rect(
        buf,
        x0 + w.saturating_sub(3),
        top,
        3,
        h,
        [base[0] * 0.48, base[1] * 0.48, base[2] * 0.54],
    );

    // 窓格子。列/行のピッチは窓寸法 + 目地。
    let px = win_w + 14;
    let py = win_h + 16;
    let mut wy = top + 26;
    while wy + win_h < HORIZON + 8 {
        let mut wx = x0 + 16;
        while wx + win_w + 16 <= x0 + w {
            // 窓ごとに明るさを変える(点灯/消灯のばらつき)。
            let k = 0.55 + 0.45 * (rng.next_u32() >> 24) as f32 / 255.0;
            fill_rect(
                buf,
                wx,
                wy,
                win_w,
                win_h,
                [win[0] * k, win[1] * k, win[2] * k],
            );
            // 窓枠の下端(水平エッジを増やす)。
            fill_rect(buf, wx, wy + win_h, win_w, 2, [30.0, 28.0, 26.0]);
            wx += px;
        }
        wy += py;
    }
}

/// シーンを浮動小数バッファに描く。
fn draw_scene() -> Vec<[f32; 3]> {
    let mut buf = vec![[0f32; 3]; (WIDTH * HEIGHT) as usize];
    let mut rng = Lcg(0x5EED_1234);

    // --- 空: 上から下へ濃青 → 淡い黄味へ。
    let sky_top = [46.0, 92.0, 168.0];
    let sky_bottom = [206.0, 214.0, 196.0];
    let ground_top = [126.0, 116.0, 96.0];
    let ground_bottom = [58.0, 50.0, 44.0];
    for y in 0..HEIGHT {
        let c = if y < HORIZON {
            let t = y as f32 / HORIZON as f32;
            mix(sky_top, sky_bottom, t * t)
        } else {
            let t = (y - HORIZON) as f32 / (HEIGHT - HORIZON) as f32;
            mix(ground_top, ground_bottom, t.sqrt())
        };
        for x in 0..WIDTH {
            put(&mut buf, x, y, c);
        }
    }

    // --- 雲(横に伸びた柔らかい帯。垂直方向のエッジは作らない)。
    for band in 0..5u32 {
        let cy = 60.0 + band as f32 * 84.0;
        let amp = 16.0 + band as f32 * 5.0;
        for y in 0..HORIZON {
            let d = (y as f32 - cy).abs();
            if d > amp {
                continue;
            }
            let k = (1.0 - d / amp) * 0.35;
            for x in 0..WIDTH {
                let wobble = ((x as f32 * 0.011 + band as f32).sin() * 0.5 + 0.5) * k;
                let p = &mut buf[(y * WIDTH + x) as usize];
                *p = mix(*p, [244.0, 246.0, 250.0], wobble);
            }
        }
    }

    // --- 遠景の建物(低コントラスト・かすみ)。
    for (i, x0) in [40u32, 300, 620, 980, 1290].into_iter().enumerate() {
        let top = HORIZON - 120 - (i as u32 % 3) * 40;
        let w = 150 + (i as u32 % 4) * 40;
        let c = [150.0 + i as f32 * 6.0, 152.0, 158.0 - i as f32 * 4.0];
        fill_rect(&mut buf, x0, top, w.min(WIDTH - x0), HORIZON - top, c);
    }

    // --- 主要な建物 4 棟(強い水平・垂直エッジ源)。
    building(
        &mut buf,
        70,
        188,
        330,
        [176.0, 92.0, 74.0],
        [236.0, 214.0, 150.0],
        34,
        44,
        &mut rng,
    );
    building(
        &mut buf,
        450,
        96,
        280,
        [78.0, 108.0, 122.0],
        [214.0, 232.0, 240.0],
        28,
        38,
        &mut rng,
    );
    building(
        &mut buf,
        780,
        250,
        360,
        [196.0, 176.0, 138.0],
        [96.0, 120.0, 148.0],
        40,
        30,
        &mut rng,
    );
    building(
        &mut buf,
        1190,
        150,
        250,
        [110.0, 128.0, 84.0],
        [240.0, 226.0, 196.0],
        30,
        46,
        &mut rng,
    );

    // --- 街灯のポール(細い垂直線)とその横木。
    for x0 in [418u32, 762, 1160] {
        fill_rect(&mut buf, x0, HORIZON - 300, 6, 320, [42.0, 40.0, 44.0]);
        fill_rect(&mut buf, x0 - 28, HORIZON - 300, 62, 7, [52.0, 48.0, 50.0]);
    }

    // --- 地面のタイル目地(水平線 + 垂直線)。
    let mut y = HORIZON + 26;
    let mut step = 14u32;
    while y < HEIGHT {
        fill_rect(&mut buf, 0, y, WIDTH, 3, [40.0, 36.0, 32.0]);
        step += 4;
        y += step;
    }
    for x in (0..WIDTH).step_by(96) {
        for yy in HORIZON..HEIGHT {
            let p = &mut buf[(yy * WIDTH + x) as usize];
            *p = mix(*p, [46.0, 42.0, 38.0], 0.5);
        }
    }
    // 地平線そのものを縁取る(最も強い水平エッジ)。
    fill_rect(&mut buf, 0, HORIZON, WIDTH, 4, [64.0, 56.0, 48.0]);

    // --- 微細テクスチャ: 粒状ノイズ + 低周波のムラ。
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let p = &mut buf[(y * WIDTH + x) as usize];
            let n = rng.noise();
            let grain = [n * 7.0, n * 6.0, n * 8.0];
            let vign = 1.0
                - 0.12
                    * (((x as f32 / WIDTH as f32) - 0.5).powi(2)
                        + ((y as f32 / HEIGHT as f32) - 0.5).powi(2));
            for c in 0..3 {
                p[c] = (p[c] + grain[c]) * vign;
            }
        }
    }

    buf
}

fn to_rgb(buf: &[[f32; 3]]) -> RgbImage {
    let mut img = RgbImage::new(WIDTH, HEIGHT);
    for (i, px) in buf.iter().enumerate() {
        let x = (i as u32) % WIDTH;
        let y = (i as u32) / WIDTH;
        img.put_pixel(
            x,
            y,
            Rgb([
                px[0].round().clamp(0.0, 255.0) as u8,
                px[1].round().clamp(0.0, 255.0) as u8,
                px[2].round().clamp(0.0, 255.0) as u8,
            ]),
        );
    }
    img
}

/// APP2 に埋め込むダミー ICC ペイロード(256 バイト、固定シードの LCG 由来)。
///
/// atx-core は ICC を「検出してコピーする」だけで中身を解釈しないため、
/// プロファイルとして妥当である必要はない。
fn dummy_icc() -> Vec<u8> {
    let mut rng = Lcg(0x1CC0_0001);
    (0..256).map(|_| rng.byte()).collect()
}

/// atx-core の `codec::encode_jpeg` と同じ設定
/// (ベースライン / 単一インターリーブスキャン / 最適化ハフマンなし)。
fn encode_jpeg(img: &RgbImage, icc: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, QUALITY);
    encoder.set_progressive(false);
    encoder.set_optimized_huffman_tables(false);
    encoder
        .add_icc_profile(icc)
        .expect("dummy icc payload must fit in one APP2 segment");
    encoder
        .encode(
            img.as_raw(),
            WIDTH as u16,
            HEIGHT as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .expect("jpeg encode");
    out
}

fn main() {
    let img = to_rgb(&draw_scene());
    let icc = dummy_icc();

    // 決定論の自己検証: 生成 → エンコードを 2 回通してバイト同一を要求する。
    let first = encode_jpeg(&img, &icc);
    let second = encode_jpeg(&to_rgb(&draw_scene()), &dummy_icc());
    assert_eq!(
        first, second,
        "fixture generation must be byte-for-byte deterministic"
    );

    assert!(
        !first.windows(6).any(|w| w == b"Exif\x00\x00"),
        "the fixture must never carry an EXIF segment"
    );
    assert!(
        first.windows(12).any(|w| w == b"ICC_PROFILE\0"),
        "the fixture must carry an APP2 ICC segment"
    );

    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/synthetic_scene.jpg");
    std::fs::create_dir_all(path.parent().unwrap()).expect("fixtures dir");
    std::fs::write(&path, &first).expect("write fixture");
    println!(
        "wrote {} ({}x{}, {} bytes)",
        path.display(),
        WIDTH,
        HEIGHT,
        first.len()
    );

    gen_tilted_fixture(&first);
}

/// `evals/fixtures/tilted_scene.jpg` を生成する。
///
/// `synthetic_scene.jpg` のバイト列(`base_jpeg`)に対し、atx-core の決定論エンジン
/// (`apply_recipe`)自身で回転 + 最大内接矩形クロップを適用し、その出力をそのまま
/// 書き出す。生成の決定論(2 回適用してバイト同一)と、傾き検出の符号・精度
/// (`detect_tilt` が ~+2.4° を十分な confidence で返すこと)の両方をここで検証する。
fn gen_tilted_fixture(base_jpeg: &[u8]) {
    let recipe = tilt_recipe();
    let limits = Limits::default();

    let first = atx_core::apply_recipe(base_jpeg, &recipe, &limits)
        .expect("apply_recipe(rotate) on synthetic scene");
    let second = atx_core::apply_recipe(base_jpeg, &recipe, &limits)
        .expect("apply_recipe(rotate) on synthetic scene (2nd run)");
    assert_eq!(
        first.bytes, second.bytes,
        "tilted fixture generation must be byte-for-byte deterministic"
    );

    // 符号・精度の自己検証: 生成した画像を detect_tilt にかけ、
    // 「時計回りに +2.4° 回すと水平になる」という想定を裏付ける。
    let decoded = image::load_from_memory(&first.bytes).expect("decode generated tilted jpeg");
    let detection = detect_tilt(&decoded, &DetectParams::default());
    let recommended = detection
        .recommended_angle_degrees
        .expect("detect_tilt should recommend a correction angle for the tilted fixture");
    println!(
        "tilted_scene.jpg: detect_tilt recommended_angle_degrees={recommended:.3} confidence={:.3} method={}",
        detection.confidence, detection.method
    );
    assert!(
        (recommended - -TILT_ROTATE_DEGREES).abs() <= 0.3,
        "expected detect_tilt to recommend ~{:.1}° to correct the {:.1}° tilt, got {recommended:.3}° ({detection:?})",
        -TILT_ROTATE_DEGREES,
        TILT_ROTATE_DEGREES
    );
    assert!(
        detection.confidence >= 0.5,
        "expected decent confidence for the tilted fixture, got {} ({detection:?})",
        detection.confidence
    );

    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../evals/fixtures/tilted_scene.jpg");
    std::fs::create_dir_all(path.parent().unwrap()).expect("evals/fixtures dir");
    std::fs::write(&path, &first.bytes).expect("write tilted fixture");
    println!(
        "wrote {} ({}x{}, {} bytes, rotate={}°)",
        path.display(),
        first.width,
        first.height,
        first.bytes.len(),
        TILT_ROTATE_DEGREES
    );
}

fn tilt_recipe() -> TransformRecipe {
    TransformRecipe {
        operations: vec![
            Operation::Rotate {
                angle_degrees: TILT_ROTATE_DEGREES,
                crop: RotateCrop::LargestInscribedRect,
            },
            Operation::Encode {
                format: OutputFormat::Jpeg,
                quality: Some(QUALITY),
                bit_depth: None,
            },
        ],
    }
}
