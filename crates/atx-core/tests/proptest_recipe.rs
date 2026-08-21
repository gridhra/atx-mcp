//! プロパティテスト: レシピの正規化・ハッシュ・検証(DESIGN §5-6)。
//!
//! - `arb_recipe()`: validate を通る(operations が構造的に正しい)レシピの生成器。
//! - `arb_any_recipe()`: 値域外・NaN・任意文字列を含む、validate に対する堅牢性確認用の生成器。

use atx_core::recipe::{
    validate, Anchor, CoordinateSpace, CropMode, Fit, Operation, OutputFormat, Rect, RotateCrop,
    StripScope, TransformRecipe,
};
use atx_core::{canonical_json, recipe_hash};
use proptest::prelude::*;

// ------------------------------------------------------------ arb_recipe

fn arb_anchor() -> impl Strategy<Value = Anchor> {
    prop_oneof![
        Just(Anchor::Center),
        Just(Anchor::Top),
        Just(Anchor::Bottom),
        Just(Anchor::Left),
        Just(Anchor::Right),
        Just(Anchor::TopLeft),
        Just(Anchor::TopRight),
        Just(Anchor::BottomLeft),
        Just(Anchor::BottomRight),
    ]
}

fn arb_crop_mode() -> impl Strategy<Value = CropMode> {
    prop_oneof![Just(CropMode::Crop), Just(CropMode::Pad)]
}

fn arb_fit() -> impl Strategy<Value = Fit> {
    prop_oneof![Just(Fit::Cover), Just(Fit::Contain), Just(Fit::Fill)]
}

fn arb_rotate_crop() -> impl Strategy<Value = RotateCrop> {
    prop_oneof![
        Just(RotateCrop::LargestInscribedRect),
        Just(RotateCrop::Full)
    ]
}

fn arb_output_format() -> impl Strategy<Value = OutputFormat> {
    prop_oneof![
        Just(OutputFormat::Jpeg),
        Just(OutputFormat::Png),
        Just(OutputFormat::Webp),
        Just(OutputFormat::Avif),
    ]
}

fn arb_strip_scope() -> impl Strategy<Value = StripScope> {
    prop_oneof![
        Just(StripScope::All),
        Just(StripScope::Gps),
        Just(StripScope::Exif)
    ]
}

fn arb_coordinate_space() -> impl Strategy<Value = CoordinateSpace> {
    prop_oneof![
        Just(CoordinateSpace::Current),
        Just(CoordinateSpace::Source)
    ]
}

fn arb_valid_pad_color() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        (any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(r, g, b)| Some(format!("#{r:02x}{g:02x}{b:02x}"))),
    ]
}

/// validate を通る単一 op(encode を除く)の生成器。
fn arb_op_no_encode() -> impl Strategy<Value = Operation> {
    prop_oneof![
        Just(Operation::AutoOrient),
        (-360.0f64..=360.0, arb_rotate_crop()).prop_map(|(angle_degrees, crop)| {
            Operation::Rotate {
                angle_degrees,
                crop,
            }
        }),
        // crop: aspect_ratio 版
        (
            (1u32..=32, 1u32..=32),
            arb_anchor(),
            arb_crop_mode(),
            arb_valid_pad_color()
        )
            .prop_map(|((w, h), anchor, mode, pad_color)| Operation::Crop {
                aspect_ratio: Some(format!("{w}:{h}")),
                rect: None,
                anchor,
                mode,
                pad_color,
                coordinate_space: CoordinateSpace::Current,
            }),
        // crop: rect 版
        (
            (0u32..64, 0u32..64, 1u32..64, 1u32..64),
            arb_anchor(),
            arb_crop_mode(),
            arb_valid_pad_color(),
            arb_coordinate_space()
        )
            .prop_map(
                |((x, y, width, height), anchor, mode, pad_color, coordinate_space)| {
                    Operation::Crop {
                        aspect_ratio: None,
                        rect: Some(Rect {
                            x,
                            y,
                            width,
                            height,
                        }),
                        anchor,
                        mode,
                        pad_color,
                        coordinate_space,
                    }
                }
            ),
        // resize: 少なくとも一方は Some、両方 0 は禁止
        (
            prop::option::of(1u32..=4096),
            prop::option::of(1u32..=4096),
            arb_fit(),
            any::<bool>()
        )
            .prop_filter("at least one dimension", |(w, h, _, _)| w.is_some()
                || h.is_some())
            .prop_map(
                |(width, height, fit, without_enlargement)| Operation::Resize {
                    width,
                    height,
                    fit,
                    without_enlargement,
                }
            ),
        (-1.0f64..=1.0, -1.0f64..=1.0, -1.0f64..=1.0, 0.0f64..=1.0).prop_map(
            |(brightness, contrast, saturation, sharpness)| Operation::Adjust {
                brightness,
                contrast,
                saturation,
                sharpness,
            }
        ),
        arb_strip_scope().prop_map(|scope| Operation::StripMetadata { scope }),
    ]
}

fn arb_encode_op() -> impl Strategy<Value = Operation> {
    (arb_output_format(), prop::option::of(1u8..=100)).prop_map(|(format, quality)| {
        Operation::Encode {
            format,
            quality,
            bit_depth: None,
        }
    })
}

/// 構造的に妥当な(validate を通る)レシピ。encode は末尾に高々1つ。
fn arb_recipe() -> impl Strategy<Value = TransformRecipe> {
    (
        prop::collection::vec(arb_op_no_encode(), 0..6),
        prop::option::of(arb_encode_op()),
    )
        .prop_map(|(mut ops, encode)| {
            match encode {
                Some(e) => ops.push(e),
                None if ops.is_empty() => ops.push(Operation::AutoOrient),
                None => {}
            }
            TransformRecipe { operations: ops }
        })
}

// ---------------------------------------------------------- arb_any_recipe

fn arb_any_f64() -> impl Strategy<Value = f64> {
    prop_oneof![
        prop::num::f64::ANY,
        Just(f64::NAN),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        -10.0f64..=10.0,
    ]
}

fn arb_any_string() -> impl Strategy<Value = String> {
    prop_oneof![
        "\\PC*",
        Just(String::new()),
        Just(":".to_string()),
        Just("W:H".to_string()),
        Just("16:9".to_string()),
        Just("0:0".to_string()),
        Just("-1:2".to_string()),
    ]
}

fn arb_any_op() -> impl Strategy<Value = Operation> {
    prop_oneof![
        Just(Operation::AutoOrient),
        (arb_any_f64(), arb_rotate_crop()).prop_map(|(angle_degrees, crop)| Operation::Rotate {
            angle_degrees,
            crop
        }),
        (
            prop::option::of(arb_any_string()),
            prop::option::of(
                (any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>()).prop_map(
                    |(x, y, width, height)| Rect {
                        x,
                        y,
                        width,
                        height
                    }
                )
            ),
            arb_anchor(),
            arb_crop_mode(),
            prop::option::of(arb_any_string()),
            arb_coordinate_space()
        )
            .prop_map(
                |(aspect_ratio, rect, anchor, mode, pad_color, coordinate_space)| {
                    Operation::Crop {
                        aspect_ratio,
                        rect,
                        anchor,
                        mode,
                        pad_color,
                        coordinate_space,
                    }
                }
            ),
        (
            prop::option::of(any::<u32>()),
            prop::option::of(any::<u32>()),
            arb_fit(),
            any::<bool>()
        )
            .prop_map(
                |(width, height, fit, without_enlargement)| Operation::Resize {
                    width,
                    height,
                    fit,
                    without_enlargement,
                }
            ),
        (arb_any_f64(), arb_any_f64(), arb_any_f64(), arb_any_f64()).prop_map(
            |(brightness, contrast, saturation, sharpness)| Operation::Adjust {
                brightness,
                contrast,
                saturation,
                sharpness,
            }
        ),
        (arb_output_format(), prop::option::of(any::<u8>())).prop_map(|(format, quality)| {
            Operation::Encode {
                format,
                quality,
                bit_depth: None,
            }
        }),
        arb_strip_scope().prop_map(|scope| Operation::StripMetadata { scope }),
    ]
}

/// 値域外・多重 encode・空 operations などを含みうる、堅牢性確認用のレシピ生成器。
fn arb_any_recipe() -> impl Strategy<Value = TransformRecipe> {
    prop::collection::vec(arb_any_op(), 0..10).prop_map(|operations| TransformRecipe { operations })
}

// -------------------------------------------------------- canonical helper

/// `write_canonical` と同じ意味論だが、オブジェクトキーを**降順**に並べる。
/// reorder-invariance の検証用(キー順序が異なっても意味が同じであることを確かめる)。
fn write_reverse(value: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
            sorted.sort_by(|a, b| b.0.cmp(a.0));
            out.push('{');
            for (i, (k, v)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*k).clone()).to_string());
                out.push(':');
                write_reverse(v, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_reverse(v, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// NOTE (fixed): f64 <-> JSON text round-trip is not always lossless with the
// pinned serde_json (see Cargo.lock; observed with 1.0.151).
//
// `serde_json::from_str`'s float parser occasionally returns a value 1 ULP away
// from the one `to_string` / `Value::to_string()` produced, even though the
// string is the exact shortest round-trippable decimal for the original value:
//   let v: f64 = 0.018742357423833362;                 // bits ...bdc125
//   let s = serde_json::to_string(&v).unwrap();         // "0.018742357423833362"
//   let back: f64 = serde_json::from_str(&s).unwrap();  // bits ...bdc124 (!= v)
// (~10% of uniform f64 in -1.0..=1.0 are affected, so it is not a rare edge.)
//
// `canonical_json` now absorbs this: non-integer f64 are quantized to the 1e-6
// grid before being written, and recipe float fields are defined to carry 1e-6
// semantic precision. Two values within ~0.5e-6 therefore produce byte-identical
// canonical JSON and the same `recipe_hash`, which is what docs/DESIGN.md §3.2
// ("同一入力 + 同一正規化レシピ → 既存 revision を返す") relies on.
//
// Consequence for property 1 below: a JSON round-trip is lossless *modulo
// quantization*, so equality is stated on the canonical form / hash rather than
// on the raw struct (a pre-quantization 1-ULP difference is intentionally
// invisible to both).
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, .. ProptestConfig::default() })]

    /// 1. canonical_json(r) は常にパース可能な JSON であり、
    ///    デシリアライズした結果は(正規化の意味で)元の r と等しい。
    ///    = 正規化形は往復に対して不動点であり、recipe_hash も変わらない。
    #[test]
    fn canonical_json_roundtrips(r in arb_recipe()) {
        prop_assume!(validate(&r).is_ok());
        let json = canonical_json(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json)
            .expect("canonical_json output must be valid JSON");
        prop_assert!(value.is_object());
        let back: TransformRecipe = serde_json::from_str(&json)
            .expect("canonical_json output must deserialize back to TransformRecipe");
        // 1e-6 未満の差(from_str の 1 ULP ずれ)は正規化で吸収されるため、
        // 生の構造体ではなく正規化形とハッシュで一致を主張する。
        prop_assert_eq!(canonical_json(&back).unwrap(), json);
        prop_assert_eq!(recipe_hash(&back).unwrap(), recipe_hash(&r).unwrap());
        // 構造(op 種別・整数/文字列フィールド)は完全に保存される。
        prop_assert_eq!(back.operations.len(), r.operations.len());
    }

    /// 2. recipe_hash(r) はシリアライズ→デシリアライズの往復で不変。
    #[test]
    fn recipe_hash_survives_roundtrip(r in arb_recipe()) {
        prop_assume!(validate(&r).is_ok());
        let h1 = recipe_hash(&r).unwrap();

        let plain = serde_json::to_string(&r).unwrap();
        let roundtripped: TransformRecipe = serde_json::from_str(&plain).unwrap();
        let h2 = recipe_hash(&roundtripped).unwrap();

        prop_assert_eq!(h1, h2);
    }

    /// 3. キー順序を(降順に)入れ替えても、デシリアライズ後のハッシュは変わらない。
    #[test]
    fn recipe_hash_is_reorder_invariant(r in arb_recipe()) {
        prop_assume!(validate(&r).is_ok());
        let h1 = recipe_hash(&r).unwrap();

        let value = serde_json::to_value(&r).unwrap();
        let mut reversed = String::new();
        write_reverse(&value, &mut reversed);

        let back: TransformRecipe = serde_json::from_str(&reversed)
            .expect("reverse-key-order JSON must still deserialize");
        let h2 = recipe_hash(&back).unwrap();

        prop_assert_eq!(h1, h2);
    }

    /// 4. validate は任意の(値域外・NaN・多重 encode 等を含む)レシピに対してパニックしない。
    #[test]
    fn validate_never_panics(r in arb_any_recipe()) {
        let _ = validate(&r);
    }
}
