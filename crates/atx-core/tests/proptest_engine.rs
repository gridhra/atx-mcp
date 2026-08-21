//! プロパティテスト: 変換エンジンの決定論・非パニック・寸法不変条件(DESIGN §5-6)。
//!
//! 画像は 8..64px の小さな合成 PNG に限定し、AVIF はコストが高いため
//! 生成対象から除外する(jpeg/png/webp のみ)。

use atx_core::recipe::{
    Anchor, CoordinateSpace, CropMode, Fit, Operation, OutputFormat, Rect, RotateCrop,
    TransformRecipe,
};
use atx_core::{apply_recipe, Limits};
use image::{ImageFormat, RgbaImage};
use proptest::prelude::*;

fn arb_image() -> impl Strategy<Value = RgbaImage> {
    (8u32..64, 8u32..64).prop_flat_map(|(w, h)| {
        prop::collection::vec(any::<u8>(), (w * h * 4) as usize)
            .prop_map(move |data| RgbaImage::from_raw(w, h, data).expect("exact buffer size"))
    })
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

fn arb_encode_format() -> impl Strategy<Value = (OutputFormat, Option<u8>)> {
    prop_oneof![
        Just((OutputFormat::Png, None)),
        (1u8..=100).prop_map(|q| (OutputFormat::Jpeg, Some(q))),
        (1u8..=100).prop_map(|q| (OutputFormat::Webp, Some(q))),
    ]
}

/// jpeg/png/webp のみを対象にした「だいたい妥当」なパイプラインレシピの生成器。
fn arb_pipeline_recipe() -> impl Strategy<Value = TransformRecipe> {
    (
        any::<bool>(),
        prop::option::of((
            -45.0f64..45.0,
            prop_oneof![
                Just(RotateCrop::LargestInscribedRect),
                Just(RotateCrop::Full)
            ],
        )),
        prop::option::of((1u32..=8, 1u32..=8)),
        prop::option::of((
            1u32..=200,
            prop_oneof![Just(Fit::Cover), Just(Fit::Contain), Just(Fit::Fill)],
        )),
        prop::option::of((-1.0f64..=1.0, -1.0f64..=1.0, -1.0f64..=1.0, 0.0f64..=1.0)),
        arb_encode_format(),
    )
        .prop_map(
            |(auto_orient, rotate, crop_ratio, resize, adjust, (format, quality))| {
                let mut ops = Vec::new();
                if auto_orient {
                    ops.push(Operation::AutoOrient);
                }
                if let Some((angle_degrees, crop)) = rotate {
                    ops.push(Operation::Rotate {
                        angle_degrees,
                        crop,
                    });
                }
                if let Some((rw, rh)) = crop_ratio {
                    ops.push(Operation::Crop {
                        aspect_ratio: Some(format!("{rw}:{rh}")),
                        rect: None,
                        anchor: Anchor::Center,
                        mode: CropMode::Crop,
                        pad_color: None,
                        coordinate_space: CoordinateSpace::Current,
                    });
                }
                if let Some((width, fit)) = resize {
                    ops.push(Operation::Resize {
                        width: Some(width),
                        height: None,
                        fit,
                        without_enlargement: false,
                    });
                }
                if let Some((brightness, contrast, saturation, sharpness)) = adjust {
                    ops.push(Operation::Adjust {
                        brightness,
                        contrast,
                        saturation,
                        sharpness,
                    });
                }
                ops.push(Operation::Encode {
                    format,
                    quality,
                    bit_depth: None,
                });
                TransformRecipe { operations: ops }
            },
        )
}

/// crop の前段に置く「幾何チェーン」(rotate 小角度 / resize / aspect crop)の生成器。
/// `coordinate_space: source` の追従性を検証するために使う。
fn arb_geometry_chain() -> impl Strategy<Value = Vec<Operation>> {
    (
        prop::option::of((
            -15.0f64..15.0,
            prop_oneof![
                Just(RotateCrop::LargestInscribedRect),
                Just(RotateCrop::Full)
            ],
        )),
        prop::option::of((
            4u32..=128,
            prop_oneof![Just(Fit::Cover), Just(Fit::Contain)],
            any::<bool>(),
        )),
        prop::option::of((1u32..=8, 1u32..=8)),
    )
        .prop_map(|(rotate, resize, crop_ratio)| {
            let mut ops = Vec::new();
            if let Some((angle_degrees, crop)) = rotate {
                ops.push(Operation::Rotate {
                    angle_degrees,
                    crop,
                });
            }
            if let Some((width, fit, without_enlargement)) = resize {
                ops.push(Operation::Resize {
                    width: Some(width),
                    height: Some(width),
                    fit,
                    without_enlargement,
                });
            }
            if let Some((rw, rh)) = crop_ratio {
                ops.push(Operation::Crop {
                    aspect_ratio: Some(format!("{rw}:{rh}")),
                    rect: None,
                    anchor: Anchor::Center,
                    mode: CropMode::Crop,
                    pad_color: None,
                    coordinate_space: CoordinateSpace::Current,
                });
            }
            ops
        })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, .. ProptestConfig::default() })]

    /// 9. SOURCE 全域を指す `coordinate_space: source` の矩形は、
    ///    前段にどんな幾何チェーンが挟まっても**決してエラーにならず**、
    ///    出力寸法はその時点の画像寸法を超えない。
    ///
    /// 「元画像の全体」は常に現在の画像と交差する(幾何 op は座標系を移動・拡縮・
    /// 回転するだけで、画像の内容は必ず写像後の矩形の内側にある)ので、
    /// 交差が空になることはあり得ない。
    #[test]
    fn source_space_full_rect_never_errors(
        img in arb_image(),
        chain in arb_geometry_chain(),
    ) {
        let (iw, ih) = img.dimensions();
        let bytes = encode_png(&img);
        let limits = Limits::default();
        let encode = Operation::Encode { format: OutputFormat::Png, quality: None, bit_depth: None };

        // 前段だけを適用したときの寸法(= crop 実行時点の「現在の寸法」)。
        let mut base_ops = chain.clone();
        base_ops.push(encode.clone());
        let base = apply_recipe(&bytes, &TransformRecipe { operations: base_ops }, &limits).unwrap();

        // 同じ前段 + SOURCE 全域を指す矩形。
        let mut ops = chain;
        ops.push(Operation::Crop {
            aspect_ratio: None,
            rect: Some(Rect { x: 0, y: 0, width: iw, height: ih }),
            anchor: Anchor::Center,
            mode: CropMode::Crop,
            pad_color: None,
            coordinate_space: CoordinateSpace::Source,
        });
        ops.push(encode);
        let out = apply_recipe(&bytes, &TransformRecipe { operations: ops }, &limits)
            .expect("a source-space rect covering the whole source image must never fail");

        prop_assert!(out.width <= base.width, "{} > {}", out.width, base.width);
        prop_assert!(out.height <= base.height, "{} > {}", out.height, base.height);
    }

    /// 5 & 6. 決定論: 同一入力+同一レシピを2回適用すると、
    ///    どちらもパニックせず(proptest がパニックを失敗として検出する)、
    ///    Ok なら結果バイト列・寸法・warnings が完全一致し、
    ///    Err ならエラーメッセージが一致する。
    #[test]
    fn apply_recipe_is_deterministic(img in arb_image(), recipe in arb_pipeline_recipe()) {
        let bytes = encode_png(&img);
        let limits = Limits::default();
        let a = apply_recipe(&bytes, &recipe, &limits);
        let b = apply_recipe(&bytes, &recipe, &limits);
        match (a, b) {
            (Ok(a), Ok(b)) => {
                prop_assert_eq!(a.bytes, b.bytes);
                prop_assert_eq!((a.width, a.height), (b.width, b.height));
                prop_assert_eq!(a.warnings, b.warnings);
            }
            (Err(e1), Err(e2)) => {
                prop_assert_eq!(e1.to_string(), e2.to_string());
            }
            other => prop_assert!(
                false,
                "apply_recipe result differs between two runs of the same input: {other:?}"
            ),
        }
    }

    /// 7a. Resize{fit: Contain} は指定ボックスを超えない。
    #[test]
    fn resize_contain_never_exceeds_box(
        img in arb_image(),
        bw in 1u32..300,
        bh in 1u32..300,
        without_enlargement in any::<bool>(),
    ) {
        let bytes = encode_png(&img);
        let recipe = TransformRecipe {
            operations: vec![
                Operation::Resize {
                    width: Some(bw),
                    height: Some(bh),
                    fit: Fit::Contain,
                    without_enlargement,
                },
                Operation::Encode { format: OutputFormat::Png, quality: None, bit_depth: None },
            ],
        };
        let out = apply_recipe(&bytes, &recipe, &Limits::default()).unwrap();
        prop_assert!(out.width <= bw, "width {} > box {}", out.width, bw);
        prop_assert!(out.height <= bh, "height {} > box {}", out.height, bh);
    }

    /// 7b. Crop{aspect_ratio: "W:H", mode: Crop} は要求比率に(丸め誤差の範囲で)一致する。
    #[test]
    fn crop_aspect_ratio_matches_target(img in arb_image(), rw in 1u32..=32, rh in 1u32..=32) {
        let bytes = encode_png(&img);
        let recipe = TransformRecipe {
            operations: vec![
                Operation::Crop {
                    aspect_ratio: Some(format!("{rw}:{rh}")),
                    rect: None,
                    anchor: Anchor::Center,
                    mode: CropMode::Crop,
                    pad_color: None,
                    coordinate_space: CoordinateSpace::Current,
                },
                Operation::Encode { format: OutputFormat::Png, quality: None, bit_depth: None },
            ],
        };
        let out = apply_recipe(&bytes, &recipe, &Limits::default()).unwrap();
        let lhs = (out.width as i64 * rh as i64 - out.height as i64 * rw as i64).abs();
        let tol = rw.max(rh) as i64;
        prop_assert!(
            lhs <= tol,
            "out {}x{} vs ratio {}:{} -> |lhs|={} tol={}",
            out.width, out.height, rw, rh, lhs, tol
        );
    }

    /// 7c. Rotate{crop: LargestInscribedRect} の出力寸法は入力寸法を超えない。
    #[test]
    fn rotate_largest_inscribed_rect_never_grows(
        img in arb_image(),
        angle in prop_oneof![1.0f64..80.0, -80.0f64..-1.0],
    ) {
        let (iw, ih) = img.dimensions();
        let bytes = encode_png(&img);
        let recipe = TransformRecipe {
            operations: vec![
                Operation::Rotate { angle_degrees: angle, crop: RotateCrop::LargestInscribedRect },
                Operation::Encode { format: OutputFormat::Png, quality: None, bit_depth: None },
            ],
        };
        let out = apply_recipe(&bytes, &recipe, &Limits::default()).unwrap();
        prop_assert!(out.width <= iw, "width {} > input {}", out.width, iw);
        prop_assert!(out.height <= ih, "height {} > input {}", out.height, ih);
    }

    /// 8. Crop{aspect_ratio} は**厳密に冪等**: 2回適用しても寸法は1回適用時と同一。
    ///
    /// かつては成り立たなかった。`pixel_ops::fit_aspect`(mode=Crop)は
    /// `current = w/h` と `target = rw/rh` の大小比較でどちらの辺を固定するかを
    /// 分岐し、自由な方の辺だけを `round()` で整数化していたため、丸めで実効比率が
    /// target を跨ぐと2回目の適用で分岐が反転し「逆の」辺が動いていた
    /// (8x8 + "1:6" → (1,8) → (1,6))。
    ///
    /// 現在は `fit_aspect_dims` が寸法計算を不動点まで反復するので、1回の適用で
    /// 安定寸法((8,8) + "1:6" → (1,6))に到達し、2回目は何も変えない。
    #[test]
    fn crop_aspect_ratio_is_idempotent(img in arb_image(), rw in 1u32..=32, rh in 1u32..=32) {
        let bytes = encode_png(&img);
        let crop_op = || Operation::Crop {
            aspect_ratio: Some(format!("{rw}:{rh}")),
            rect: None,
            anchor: Anchor::Center,
            mode: CropMode::Crop,
            pad_color: None,
            coordinate_space: CoordinateSpace::Current,
        };
        let once = TransformRecipe {
            operations: vec![crop_op(), Operation::Encode { format: OutputFormat::Png, quality: None, bit_depth: None }],
        };
        let twice = TransformRecipe {
            operations: vec![
                crop_op(),
                crop_op(),
                Operation::Encode { format: OutputFormat::Png, quality: None, bit_depth: None },
            ],
        };
        let a = apply_recipe(&bytes, &once, &Limits::default()).unwrap();
        let b = apply_recipe(&bytes, &twice, &Limits::default()).unwrap();
        prop_assert_eq!((a.width, a.height), (b.width, b.height));
        // 2回目のクロップは恒等変換なので画素も完全に一致する。
        prop_assert_eq!(a.bytes, b.bytes);
    }
}
