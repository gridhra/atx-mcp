//! レシピ語彙(op)のリファレンス表。`list_operations` / `explain_operation` の唯一の情報源。
//!
//! ROADMAP §Agent UX の規律 #2「語彙の段階的開示」の実装:
//! `apply_transform` の inputSchema に全 op を埋め込む代わりに、
//! 軽量カタログ(`list_operations`)と完全な仕様 + 例(`explain_operation`)を
//! オンデマンドのツールとして提供する。
//!
//! ここの内容は `atx_core::recipe::Operation`(serde 契約)と
//! 各 op の `validate` に対して手で同期させている。op を足すときは
//! [`OPERATIONS`] にも1エントリ足すこと(件数はテストで固定している)。

/// 1つの op パラメータの説明。
#[derive(Debug, Clone, Copy)]
pub struct ParamDoc {
    /// JSON のフィールド名。
    pub name: &'static str,
    /// 型と値域の簡潔な表記(例: `"f64 -360..360"`, `"enum cover|contain|fill"`)。
    pub type_hint: &'static str,
    /// `"required"` / `"default: center"` / `"optional"` のいずれか。
    pub requirement: &'static str,
    /// 意味の説明(explain_operation でのみ返す)。
    pub semantics: &'static str,
}

impl ParamDoc {
    /// カタログ用の1項目表記(必須は `*`、任意は `?`、既定値は `=` で示す)。
    ///
    /// 既定値が enum の候補そのものである場合は `=x` を繰り返さず、
    /// 候補側に `(def)` を付けて短く書く(カタログのトークン費削減)。
    pub fn compact(&self) -> String {
        match self.requirement.strip_prefix("default: ") {
            Some(default) => {
                let marked = mark_enum_default(self.type_hint, default);
                match marked {
                    Some(hint) => format!("{}: {hint}", self.name),
                    None => format!("{}: {} ={default}", self.name, self.type_hint),
                }
            }
            None if self.requirement == "required" => {
                format!("{}*: {}", self.name, self.type_hint)
            }
            None => format!("{}?: {}", self.name, self.type_hint),
        }
    }
}

/// 1つの op の説明。
#[derive(Debug, Clone, Copy)]
pub struct OpDoc {
    /// `{"op": "..."}` に書く名前。
    pub name: &'static str,
    /// `list_operations` の `category` で絞り込める分類。
    pub category: &'static str,
    /// 1行の英語説明。
    pub summary: &'static str,
    pub params: &'static [ParamDoc],
    /// そのまま `operations` に入れられる JSON 断片(explain_operation 用)。
    pub examples: &'static [&'static str],
    /// 落とし穴・注意点(explain_operation 用)。
    pub warnings: &'static [&'static str],
}

/// `list_operations` で使える category の一覧。
pub const CATEGORIES: [&str; 4] = ["geometry", "color", "filter", "output"];

/// レシピ語彙の全 op(`atx_core::recipe::Operation` と1対1)。
pub const OPERATIONS: &[OpDoc] = &[
    OpDoc {
        name: "auto_orient",
        category: "geometry",
        summary: "Normalize EXIF orientation (a no-op: done at decode).",
        params: &[],
        examples: &[r#"{"op": "auto_orient"}"#],
        warnings: &[
            "atx normalizes EXIF Orientation into the pixels at decode time, so this op never changes anything. It exists only to make the intent explicit in a recipe.",
        ],
    },
    OpDoc {
        name: "rotate",
        category: "geometry",
        summary: "Rotate by any angle (positive = clockwise).",
        params: &[
            ParamDoc {
                name: "angle_degrees",
                type_hint: "f64 -360..360",
                requirement: "required",
                semantics: "Rotation in degrees; positive rotates clockwise. Must be finite.",
            },
            ParamDoc {
                name: "crop",
                type_hint: "enum largest_inscribed_rect|full",
                requirement: "default: largest_inscribed_rect",
                semantics: "largest_inscribed_rect crops to the biggest rectangle fully inside the rotated image (no padding, but the image gets smaller). full keeps the whole rotated canvas and fills the corners with padding.",
            },
        ],
        examples: &[
            r#"{"op": "rotate", "angle_degrees": -1.8}"#,
            r#"{"op": "rotate", "angle_degrees": 90, "crop": "full"}"#,
        ],
        warnings: &[
            "The default crop=largest_inscribed_rect SHRINKS the image: a small tilt correction on a 4000x3000 photo loses a few percent on every side. Inspect the resulting dimensions, and use crop=\"full\" when you must keep every pixel.",
            "Rotation resamples pixels; chaining many rotate ops softens the image. Prefer one rotate with the summed angle.",
        ],
    },
    OpDoc {
        name: "crop",
        category: "geometry",
        summary: "Crop or pad to an aspect ratio or an explicit rect.",
        params: &[
            ParamDoc {
                name: "aspect_ratio",
                type_hint: "\"W:H\"",
                requirement: "optional",
                semantics: "Target ratio such as \"16:9\". Exactly one of aspect_ratio or rect must be given.",
            },
            ParamDoc {
                name: "rect",
                type_hint: "{x,y,w,h} u32",
                requirement: "optional",
                semantics: "Explicit pixel rectangle {x, y, width, height}; width/height must be > 0. Exactly one of aspect_ratio or rect must be given.",
            },
            ParamDoc {
                name: "anchor",
                type_hint: "enum, 9 anchors",
                requirement: "default: center",
                semantics: "One of center|top|bottom|left|right|top_left|top_right|bottom_left|bottom_right. Chooses which part is kept (mode=crop), or where the image sits inside the padded canvas (mode=pad). Only meaningful with aspect_ratio.",
            },
            ParamDoc {
                name: "mode",
                type_hint: "enum crop|pad",
                requirement: "default: crop",
                semantics: "crop cuts the overflow away; pad adds margin in pad_color to reach the ratio (nothing is lost).",
            },
            ParamDoc {
                name: "pad_color",
                type_hint: "css hex",
                requirement: "default: #ffffff",
                semantics: "Fill color used when mode=pad, as CSS hex: #rgb, #rrggbb or #rrggbbaa.",
            },
            ParamDoc {
                name: "coordinate_space",
                type_hint: "enum current|source",
                requirement: "default: current",
                semantics: "current interprets rect in the image as it is at this point of the pipeline. source interprets it in the ORIGINAL input image (before EXIF orientation normalization); the engine maps the rect through the accumulated geometric transform.",
            },
        ],
        examples: &[
            r#"{"op": "crop", "aspect_ratio": "16:9", "anchor": "center"}"#,
            r#"{"op": "crop", "rect": {"x": 120, "y": 80, "width": 1600, "height": 900}, "coordinate_space": "source"}"#,
        ],
        warnings: &[
            "aspect_ratio and rect are mutually exclusive and one of them is required.",
            "coordinate_space=\"source\" is only valid together with rect. After a rotate/perspective, the four mapped corners are replaced by their AXIS-ALIGNED BOUNDING BOX, so the crop is slightly larger than the tilted quad you drew.",
            "A source-space rect is rounded half-away-from-zero and clamped to the current image; clamping is reported in warnings, and an empty intersection is a structured error.",
        ],
    },
    OpDoc {
        name: "resize",
        category: "geometry",
        summary: "Resize to a width and/or height with a fit policy.",
        params: &[
            ParamDoc {
                name: "width",
                type_hint: "u32 > 0",
                requirement: "optional",
                semantics: "Target width. At least one of width or height is required.",
            },
            ParamDoc {
                name: "height",
                type_hint: "u32 > 0",
                requirement: "optional",
                semantics: "Target height. At least one of width or height is required.",
            },
            ParamDoc {
                name: "fit",
                type_hint: "enum cover|contain|fill",
                requirement: "default: cover",
                semantics: "cover fills the box and crops the overflow; contain fits inside the box keeping the ratio; fill stretches to the exact size, ignoring the ratio.",
            },
            ParamDoc {
                name: "without_enlargement",
                type_hint: "bool",
                requirement: "default: true",
                semantics: "When true, an image smaller than the target is left as-is instead of being upscaled.",
            },
        ],
        examples: &[
            r#"{"op": "resize", "width": 1600, "fit": "cover"}"#,
            r#"{"op": "resize", "width": 2000, "height": 2000, "fit": "contain", "without_enlargement": true}"#,
        ],
        warnings: &[
            "without_enlargement defaults to true, so asking for a bigger size than the source silently does nothing. Pass false when you really want upscaling.",
            "With only one of width/height, the other is derived from the aspect ratio.",
        ],
    },
    OpDoc {
        name: "adjust",
        category: "color",
        summary: "Quick brightness/contrast/saturation/sharpness tweaks.",
        params: &[
            ParamDoc {
                name: "brightness",
                type_hint: "f64 -1..1",
                requirement: "default: 0",
                semantics: "0 = unchanged; positive brightens.",
            },
            ParamDoc {
                name: "contrast",
                type_hint: "f64 -1..1",
                requirement: "default: 0",
                semantics: "0 = unchanged; positive increases contrast around mid gray.",
            },
            ParamDoc {
                name: "saturation",
                type_hint: "f64 -1..1",
                requirement: "default: 0",
                semantics: "0 = unchanged; -1 is fully desaturated.",
            },
            ParamDoc {
                name: "sharpness",
                type_hint: "f64 0..1",
                requirement: "default: 0",
                semantics: "0 = unchanged. Note the range starts at 0, unlike the other three.",
            },
        ],
        examples: &[
            r#"{"op": "adjust", "brightness": 0.05, "contrast": 0.1}"#,
            r#"{"op": "adjust", "saturation": -1.0}"#,
        ],
        warnings: &[
            "sharpness is 0..1, the others are -1..1; out-of-range values are a validation error, not a clamp.",
            "For precise tone work prefer curves/levels; for a full desaturate prefer the grayscale preset (BT.709 luma) over saturation=-1.",
        ],
    },
    OpDoc {
        name: "perspective",
        category: "geometry",
        summary: "Keystone/perspective correction by quad or angles.",
        params: &[
            ParamDoc {
                name: "quad",
                type_hint: "[[f64;2];4]",
                requirement: "optional",
                semantics: "Four source-image pixel corners (top-left, top-right, bottom-right, bottom-left) that should become the output rectangle.",
            },
            ParamDoc {
                name: "vertical_degrees",
                type_hint: "f64 deg",
                requirement: "optional",
                semantics: "Vertical keystone angle; positive means the top edge leans away (corrects a converging top).",
            },
            ParamDoc {
                name: "horizontal_degrees",
                type_hint: "f64 deg",
                requirement: "optional",
                semantics: "Horizontal keystone angle.",
            },
            ParamDoc {
                name: "pad_color",
                type_hint: "css hex",
                requirement: "default: #ffffff",
                semantics: "Fill color for areas pulled in from outside the source.",
            },
        ],
        examples: &[
            r#"{"op": "perspective", "vertical_degrees": 4.5}"#,
            r#"{"op": "perspective", "quad": [[120,60],[1880,90],[1900,1180],[100,1150]]}"#,
        ],
        warnings: &[
            "The two forms are mutually exclusive: either quad, or one/both of vertical_degrees / horizontal_degrees.",
            "This is a projective transform, so the corners of the result are filled with pad_color unless you crop afterwards.",
            "detect_tilt reports horizontal- and vertical-line families separately; a disagreement between them is the signal that you want perspective rather than rotate.",
        ],
    },
    OpDoc {
        name: "color_matrix",
        category: "color",
        summary: "4x5 color matrix (B&W, sepia, hue rotate, channel mixer).",
        params: &[ParamDoc {
            name: "matrix",
            type_hint: "f64[20]",
            requirement: "required",
            semantics: "Row-major 4x5 matrix M with [R',G',B',A'] = M * [R,G,B,A,1], applied to values normalized to 0..1 and then clamped. The 5th column of each row is a constant offset in 0..1 units.",
        }],
        examples: &[
            r#"{"op": "color_matrix", "matrix": [0.2126,0.7152,0.0722,0,0, 0.2126,0.7152,0.0722,0,0, 0.2126,0.7152,0.0722,0,0, 0,0,0,1,0]}"#,
            r#"{"op": "color_matrix", "matrix": [0.393,0.769,0.189,0,0, 0.349,0.686,0.168,0,0, 0.272,0.534,0.131,0,0, 0,0,0,1,0]}"#,
        ],
        warnings: &[
            "The matrix must have exactly 20 elements; the identity is [1,0,0,0,0, 0,1,0,0,0, 0,0,1,0,0, 0,0,0,1,0].",
            "Alpha is part of the matrix. Keep the last row as 0,0,0,1,0 unless you intend to change transparency.",
            "The two examples above are exactly the built-in grayscale and sepia presets; use preset=\"grayscale\" / \"sepia\" instead of retyping them.",
        ],
    },
    OpDoc {
        name: "curves",
        category: "color",
        summary: "Per-channel tone curve from control points.",
        params: &[
            ParamDoc {
                name: "master",
                type_hint: "[[u8;2]]",
                requirement: "optional",
                semantics: "Control points [x, y] with x,y in 0..255, applied to R, G and B before the per-channel curves.",
            },
            ParamDoc {
                name: "red",
                type_hint: "[[u8;2]]",
                requirement: "optional",
                semantics: "Control points applied to the red channel only.",
            },
            ParamDoc {
                name: "green",
                type_hint: "[[u8;2]]",
                requirement: "optional",
                semantics: "Control points applied to the green channel only.",
            },
            ParamDoc {
                name: "blue",
                type_hint: "[[u8;2]]",
                requirement: "optional",
                semantics: "Control points applied to the blue channel only.",
            },
        ],
        examples: &[
            r#"{"op": "curves", "master": [[0,0],[64,50],[192,205],[255,255]]}"#,
            r#"{"op": "curves", "blue": [[0,12],[255,243]]}"#,
        ],
        warnings: &[
            "Control point x values must be strictly increasing; duplicate x is a validation error.",
            "Order matters: master runs first, then the per-channel curves on the result.",
            "An omitted channel is the identity; a single point makes that channel constant.",
        ],
    },
    OpDoc {
        name: "levels",
        category: "color",
        summary: "Black/white points plus gamma (sugar over curves).",
        params: &[
            ParamDoc {
                name: "in_black",
                type_hint: "u8 0..255",
                requirement: "default: 0",
                semantics: "Input level mapped to out_black.",
            },
            ParamDoc {
                name: "in_white",
                type_hint: "u8 0..255",
                requirement: "default: 255",
                semantics: "Input level mapped to out_white; must be above in_black.",
            },
            ParamDoc {
                name: "gamma",
                type_hint: "f64 0.1..10",
                requirement: "default: 1.0",
                semantics: "Midtone gamma; 1.0 leaves midtones alone, > 1 brightens them.",
            },
            ParamDoc {
                name: "out_black",
                type_hint: "u8 0..255",
                requirement: "default: 0",
                semantics: "Output level for in_black.",
            },
            ParamDoc {
                name: "out_white",
                type_hint: "u8 0..255",
                requirement: "default: 255",
                semantics: "Output level for in_white.",
            },
        ],
        examples: &[
            r#"{"op": "levels", "in_black": 12, "in_white": 240}"#,
            r#"{"op": "levels", "gamma": 1.2}"#,
        ],
        warnings: &[
            "Everything defaults to the identity, so an empty levels op does nothing.",
            "It compiles down to the same 256-entry LUT path as curves; use curves when you need shaped, non-linear control.",
        ],
    },
    OpDoc {
        name: "blur",
        category: "filter",
        summary: "Gaussian blur with a given sigma.",
        params: &[ParamDoc {
            name: "sigma",
            type_hint: "f64 0.1..100",
            requirement: "required",
            semantics: "Standard deviation in pixels; the kernel radius is ceil(3*sigma).",
        }],
        examples: &[
            r#"{"op": "blur", "sigma": 2.0}"#,
            r#"{"op": "blur", "sigma": 12.0}"#,
        ],
        warnings: &[
            "Cost grows with sigma. Blur AFTER resizing when you can: the same visual softness needs a much smaller sigma on a smaller image.",
            "The alpha channel is blurred too.",
        ],
    },
    OpDoc {
        name: "median",
        category: "filter",
        summary: "Median filter (speckle/noise removal).",
        params: &[ParamDoc {
            name: "radius",
            type_hint: "u32 1..16",
            requirement: "required",
            semantics: "Half-width in pixels; the window is (2*radius+1)^2 pixels.",
        }],
        examples: &[
            r#"{"op": "median", "radius": 2}"#,
            r#"{"op": "median", "radius": 5}"#,
        ],
        warnings: &[
            "Cost is O(radius^2) per pixel; radius above ~5 is slow on large images.",
            "It removes fine texture along with the noise. Preview before committing.",
        ],
    },
    OpDoc {
        name: "unsharp_mask",
        category: "filter",
        summary: "Unsharp mask sharpening.",
        params: &[
            ParamDoc {
                name: "amount",
                type_hint: "f64 0..4",
                requirement: "required",
                semantics: "Strength of the added high-frequency detail; 0 leaves the image unchanged.",
            },
            ParamDoc {
                name: "radius",
                type_hint: "f64 0.1..50",
                requirement: "required",
                semantics: "Sigma of the internal gaussian blur, in pixels.",
            },
            ParamDoc {
                name: "threshold",
                type_hint: "u8 0..255",
                requirement: "default: 0",
                semantics: "Per-channel absolute difference below which a pixel is left untouched, protecting flat areas such as sky and skin.",
            },
        ],
        examples: &[
            r#"{"op": "unsharp_mask", "amount": 0.8, "radius": 1.2, "threshold": 3}"#,
            r#"{"op": "unsharp_mask", "amount": 1.5, "radius": 2.0}"#,
        ],
        warnings: &[
            "threshold compares per-channel differences, not luminance (a deliberate simplification for determinism).",
            "Sharpen LAST, after the final resize; sharpening before downscaling produces halos.",
        ],
    },
    OpDoc {
        name: "encode",
        category: "output",
        summary: "Output format/quality. At most one, and it must be last.",
        params: &[
            ParamDoc {
                name: "format",
                type_hint: "enum jpeg|png|webp|avif",
                requirement: "required",
                semantics: "Output container/codec.",
            },
            ParamDoc {
                name: "quality",
                type_hint: "u8 1..100",
                requirement: "optional",
                semantics: "Lossy quality; ignored for png.",
            },
        ],
        examples: &[
            r#"{"op": "encode", "format": "webp", "quality": 82}"#,
            r#"{"op": "encode", "format": "png"}"#,
        ],
        warnings: &[
            "encode must be the LAST operation and may appear at most once; omitting it keeps the input format.",
            "For png/webp/avif output any ICC profile on the source is dropped (embedding is jpeg-only) and reported as a warning, not an error.",
            "render_preview ignores your encode op: it always returns a JPEG q80 downscaled to a long edge of 768.",
        ],
    },
    OpDoc {
        name: "strip_metadata",
        category: "output",
        summary: "Remove metadata (EXIF/GPS; ICC optionally kept).",
        params: &[ParamDoc {
            name: "scope",
            type_hint: "enum all|gps|exif",
            requirement: "default: all",
            semantics: "all drops every metadata block including ICC; exif drops EXIF (GPS included) but keeps the ICC profile; gps behaves like all in v1.",
        }],
        examples: &[
            r#"{"op": "strip_metadata", "scope": "exif"}"#,
            r#"{"op": "strip_metadata"}"#,
        ],
        warnings: &[
            "scope=\"gps\" is currently a superset: it drops the whole EXIF block, not only the GPS tags.",
            "scope=\"exif\" only preserves ICC for jpeg output; other formats drop it anyway with a warning.",
            "inspect_image reports has_gps, so check before deciding whether stripping is needed.",
        ],
    },
];

/// `"enum a|b|c"` の候補 `default` に `(def)` を付けた表記を返す。
/// enum でない、または候補に含まれない場合は `None`。
fn mark_enum_default(type_hint: &str, default: &str) -> Option<String> {
    let variants = type_hint.strip_prefix("enum ")?;
    if !variants.split('|').any(|v| v == default) {
        return None;
    }
    let marked: Vec<String> = variants
        .split('|')
        .map(|v| {
            if v == default {
                format!("{v}(def)")
            } else {
                v.to_string()
            }
        })
        .collect();
    Some(format!("enum {}", marked.join("|")))
}

/// 名前から op を引く。
pub fn find(name: &str) -> Option<&'static OpDoc> {
    OPERATIONS.iter().find(|op| op.name == name)
}

/// 全 op 名(定義順)。
pub fn operation_names() -> Vec<&'static str> {
    OPERATIONS.iter().map(|op| op.name).collect()
}

/// 「もしかして」候補: 前方一致 / 部分一致 / 1文字違い程度の素朴な近傍探索。
pub fn did_you_mean(given: &str) -> Vec<&'static str> {
    let given_lower = given.to_ascii_lowercase();
    OPERATIONS
        .iter()
        .filter(|op| {
            op.name.starts_with(&given_lower)
                || given_lower.starts_with(op.name)
                || op.name.contains(&given_lower)
                || given_lower.contains(op.name)
        })
        .map(|op| op.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_every_operation_exactly_once() {
        let names = operation_names();
        assert_eq!(names.len(), 14, "v0.2 has 14 operations");
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "op names must be unique");
        for op in OPERATIONS {
            assert!(
                CATEGORIES.contains(&op.category),
                "{}: unknown category {}",
                op.name,
                op.category
            );
            assert!(!op.summary.is_empty(), "{}: needs a summary", op.name);
            assert!(!op.examples.is_empty(), "{}: needs an example", op.name);
            for param in op.params {
                assert!(
                    param.requirement == "required"
                        || param.requirement == "optional"
                        || param.requirement.starts_with("default: "),
                    "{}.{}: bad requirement {:?}",
                    op.name,
                    param.name,
                    param.requirement
                );
            }
        }
    }

    /// 例は本当に `Operation` としてデシリアライズできること
    /// (`deny_unknown_fields` なのでフィールド名の誤りもここで落ちる)。
    #[test]
    fn every_example_deserializes_as_an_operation() {
        for op in OPERATIONS {
            for example in op.examples {
                let parsed: atx_core::recipe::Operation = serde_json::from_str(example)
                    .unwrap_or_else(|e| panic!("{}: example {example} must parse: {e}", op.name));
                let tag = serde_json::to_value(&parsed).unwrap();
                assert_eq!(
                    tag["op"], op.name,
                    "{}: example must use its own op tag",
                    op.name
                );
            }
        }
    }
}
