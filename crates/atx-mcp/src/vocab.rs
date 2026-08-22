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

/// 局所適用マスク(v0.5)の共有パラメータ説明。
///
/// トーン系・フィルタ系の 14 op が同じ `mask` フィールドを取るので、
/// **説明は1箇所だけ**持ち、各 op の `params` から同じ定数を参照する。
/// カタログ(`list_operations`)には `compact()` の1行
/// `mask?: {revision_id,invert,feather_px}` だけが出て、
/// 下の詳細は `explain_operation` の params 表にだけ現れる
/// (ROADMAP §Agent UX の規律 #2「語彙の段階的開示」= カタログの予算を守る)。
pub const MASK_PARAM: ParamDoc = ParamDoc {
    name: "mask",
    type_hint: "{revision_id,invert,feather_px}",
    requirement: "optional",
    semantics: "Applies this operation only where a mask says so, instead of over the whole image. \
        revision_id (required) is a GRAYSCALE IMAGE revision in this workspace whose BT.709 luma is the weight: \
        white = the operation applies at full strength, black = the pixel is left untouched, grey = a linear blend of the two. \
        invert (default false) flips the weight to 1-w. feather_px (default 0.0, up to 200.0) blurs the mask edge by that gaussian sigma, in pixels of the CURRENT image. \
        A mask whose dimensions differ from the current image is resampled to fit, so build it against the same revision you are transforming. \
        Get a mask from generate_mask (linear_gradient / radial_gradient / luminosity_range / color_range) or import_asset your own, \
        and check its coverage with render_preview overlay=\"mask\". The referenced revision id is part of the recipe hash, so the recipe only reproduces inside a workspace that holds that mask.",
};

/// 14 の調整・フィルタ op に共通で付ける `mask` の注意書き(実体は1つ)。
pub const MASK_WARNING: &str = "Optional `mask` restricts this operation to part of the image \
    (white = full strength, black = untouched). Order matters: masked and unmasked ops still run \
    strictly in sequence, and a geometric op (rotate/crop/resize/perspective) after a masked op \
    moves the pixels but not the mask you already applied - put the geometry first when you can.";

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
        MASK_PARAM,
        ],
        examples: &[
            r#"{"op": "adjust", "brightness": 0.05, "contrast": 0.1}"#,
            r#"{"op": "adjust", "saturation": -1.0}"#,
        ],
        warnings: &[
            "sharpness is 0..1, the others are -1..1; out-of-range values are a validation error, not a clamp.",
            "For precise tone work prefer curves/levels; for a full desaturate prefer the grayscale preset (BT.709 luma) over saturation=-1.",
                    MASK_WARNING,
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
                type_hint: "f64 -45..45",
                requirement: "optional",
                semantics: "Vertical keystone angle in degrees, limited to -45..=45; positive means the top edge leans away (corrects a converging top). Outside that range is a validation error, not a clamp.",
            },
            ParamDoc {
                name: "horizontal_degrees",
                type_hint: "f64 -45..45",
                requirement: "optional",
                semantics: "Horizontal keystone angle in degrees, limited to -45..=45 exactly like vertical_degrees.",
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
            "quad must be a STRICTLY CONVEX quadrilateral listed in the order top-left, top-right, bottom-right, bottom-left (clockwise on screen, y pointing down). A concave, self-intersecting, degenerate (3 collinear points) or reversed-order quad is a hard validation error, not a silently mirrored result.",
            "vertical_degrees / horizontal_degrees are each limited to -45..=45 degrees; a larger angle is a validation error.",
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
            type_hint: "f64[20] |v|<=8",
            requirement: "required",
            semantics: "Row-major 4x5 matrix M with [R',G',B',A'] = M * [R,G,B,A,1], applied to values normalized to 0..1 and then clamped. The 5th column of each row is a constant offset in 0..1 units. Every element must be finite with |v| <= 8.0 (normal color matrices sit within -2..2; 8 leaves room for a strong channel mixer).",
        },
        MASK_PARAM,
        ],
        examples: &[
            r#"{"op": "color_matrix", "matrix": [0.2126,0.7152,0.0722,0,0, 0.2126,0.7152,0.0722,0,0, 0.2126,0.7152,0.0722,0,0, 0,0,0,1,0]}"#,
            r#"{"op": "color_matrix", "matrix": [0.393,0.769,0.189,0,0, 0.349,0.686,0.168,0,0, 0.272,0.534,0.131,0,0, 0,0,0,1,0]}"#,
        ],
        warnings: &[
            "The matrix must have exactly 20 elements, each finite with |v| <= 8.0; the identity is [1,0,0,0,0, 0,1,0,0,0, 0,0,1,0,0, 0,0,0,1,0].",
            "Alpha is part of the matrix. Keep the last row as 0,0,0,1,0 unless you intend to change transparency.",
            "The two examples above are exactly the built-in grayscale and sepia presets; use preset=\"grayscale\" / \"sepia\" instead of retyping them.",
                    MASK_WARNING,
        ],
    },
    OpDoc {
        name: "curves",
        category: "color",
        summary: "Per-channel tone curve from control points.",
        params: &[
            ParamDoc {
                name: "master",
                type_hint: "[[u8;2]] 1..32 pts",
                requirement: "optional",
                semantics: "1 to 32 control points [x, y] with x,y in 0..255, applied to R, G and B before the per-channel curves.",
            },
            ParamDoc {
                name: "red",
                type_hint: "[[u8;2]] 1..32 pts",
                requirement: "optional",
                semantics: "1 to 32 control points applied to the red channel only.",
            },
            ParamDoc {
                name: "green",
                type_hint: "[[u8;2]] 1..32 pts",
                requirement: "optional",
                semantics: "1 to 32 control points applied to the green channel only.",
            },
            ParamDoc {
                name: "blue",
                type_hint: "[[u8;2]] 1..32 pts",
                requirement: "optional",
                semantics: "1 to 32 control points applied to the blue channel only.",
            },
        MASK_PARAM,
        ],
        examples: &[
            r#"{"op": "curves", "master": [[0,0],[64,50],[192,205],[255,255]]}"#,
            r#"{"op": "curves", "blue": [[0,12],[255,243]]}"#,
        ],
        warnings: &[
            "Control point x values must be strictly increasing; duplicate x is a validation error.",
            "Each channel takes 1..=32 control points; an empty list or more than 32 points is a validation error.",
            "Order matters: master runs first, then the per-channel curves on the result.",
            "An omitted channel is the identity; a single point makes that channel constant.",
                    MASK_WARNING,
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
        MASK_PARAM,
        ],
        examples: &[
            r#"{"op": "levels", "in_black": 12, "in_white": 240}"#,
            r#"{"op": "levels", "gamma": 1.2}"#,
        ],
        warnings: &[
            "Everything defaults to the identity, so an empty levels op does nothing.",
            "in_black must be strictly less than in_white, but the OUTPUT range only has to satisfy out_black <= out_white: an inverted output range is a validation error, while a crushed one (out_black == out_white, a flat fill) is allowed on purpose.",
            "It compiles down to the same 256-entry LUT path as curves; use curves when you need shaped, non-linear control.",
                    MASK_WARNING,
        ],
    },
    OpDoc {
        name: "lut",
        category: "color",
        summary: "Apply an imported .cube 3D/1D LUT to the image.",
        params: &[
            ParamDoc {
                name: "lut_revision_id",
                type_hint: "\"rev_...\"",
                requirement: "required",
                semantics: "Revision id of a .cube LUT already imported into THIS workspace. Workflow: call import_asset on the .cube file first (it becomes a revision with mime_type application/x-cube), then put the returned revision_id here.",
            },
            ParamDoc {
                name: "strength",
                type_hint: "f64 0..1",
                requirement: "default: 1.0",
                semantics: "Linear blend between the original (0.0) and the fully LUT-mapped image (1.0).",
            },
        MASK_PARAM,
        ],
        examples: &[
            r#"{"op": "lut", "lut_revision_id": "rev_01J000000000000000000000"}"#,
            r#"{"op": "lut", "lut_revision_id": "rev_01J000000000000000000000", "strength": 0.6}"#,
        ],
        warnings: &[
            "The LUT must be in the workspace: import_asset the .cube file FIRST, then reference the revision_id it returns. A recipe pointing at an unknown id fails with a structured error before any pixel work happens.",
            "inspect_image refuses a .cube revision on purpose - it is an asset, not an image.",
            "The recipe hash includes the referenced revision id, so a recipe is only reproducible inside a workspace that holds that LUT. Export/import the .cube alongside the recipe to move a look between machines.",
            "3D LUTs use tetrahedral interpolation; 1D LUTs are interpolated linearly.",
                    MASK_WARNING,
        ],
    },
    OpDoc {
        name: "white_balance",
        category: "color",
        summary: "White balance: temperature and tint shift.",
        params: &[
            ParamDoc {
                name: "temperature",
                type_hint: "f64 -100..100",
                requirement: "default: 0",
                semantics: "0 = unchanged; positive warms the image (towards amber), negative cools it (towards blue).",
            },
            ParamDoc {
                name: "tint",
                type_hint: "f64 -100..100",
                requirement: "default: 0",
                semantics: "0 = unchanged; positive shifts towards magenta, negative towards green.",
            },
        MASK_PARAM,
        ],
        examples: &[
            r#"{"op": "white_balance", "temperature": 12, "tint": -4}"#,
            r#"{"op": "white_balance", "temperature": -20}"#,
        ],
        warnings: &[
            "This is an sRGB channel-gain approximation of the Lightroom sliders, not a colorimetric chromatic-adaptation transform; it is monotonic and deterministic but not physically exact.",
            "Large shifts clip highlights in the boosted channels. Preview before committing.",
                    MASK_WARNING,
        ],
    },
    OpDoc {
        name: "hsl",
        category: "color",
        summary: "Per-hue-band HSL shifts (Lightroom HSL panel).",
        params: &[
            ParamDoc {
                name: "red",
                type_hint: "{hue,saturation,luminance}",
                requirement: "optional",
                semantics: "Shifts for the red band; each field is -100..100 and defaults to 0. Omitting the band leaves it untouched.",
            },
            ParamDoc {
                name: "orange",
                type_hint: "{hue,saturation,luminance}",
                requirement: "optional",
                semantics: "Same shape as red, for the orange band (skin tones live mostly here).",
            },
            ParamDoc {
                name: "yellow",
                type_hint: "{hue,saturation,luminance}",
                requirement: "optional",
                semantics: "Same shape as red, for the yellow band.",
            },
            ParamDoc {
                name: "green",
                type_hint: "{hue,saturation,luminance}",
                requirement: "optional",
                semantics: "Same shape as red, for the green band (foliage).",
            },
            ParamDoc {
                name: "aqua",
                type_hint: "{hue,saturation,luminance}",
                requirement: "optional",
                semantics: "Same shape as red, for the aqua/cyan band.",
            },
            ParamDoc {
                name: "blue",
                type_hint: "{hue,saturation,luminance}",
                requirement: "optional",
                semantics: "Same shape as red, for the blue band (sky, water).",
            },
            ParamDoc {
                name: "purple",
                type_hint: "{hue,saturation,luminance}",
                requirement: "optional",
                semantics: "Same shape as red, for the purple band.",
            },
            ParamDoc {
                name: "magenta",
                type_hint: "{hue,saturation,luminance}",
                requirement: "optional",
                semantics: "Same shape as red, for the magenta band.",
            },
        MASK_PARAM,
        ],
        examples: &[
            r#"{"op": "hsl", "blue": {"saturation": 15, "luminance": -8}}"#,
            r#"{"op": "hsl", "orange": {"hue": -6, "saturation": -10}, "green": {"saturation": -20}}"#,
        ],
        warnings: &[
            "Every field of every band is -100..100; out-of-range values are a validation error, not a clamp.",
            "Band boundaries are feathered, so a shift on one band bleeds slightly into its neighbours. For a global change use adjust or color_matrix instead.",
            "hue is a shift towards the neighbouring hue, not an absolute hue value.",
                    MASK_WARNING,
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
        },
        MASK_PARAM,
        ],
        examples: &[
            r#"{"op": "blur", "sigma": 2.0}"#,
            r#"{"op": "blur", "sigma": 12.0}"#,
        ],
        warnings: &[
            "Cost grows with sigma. Blur AFTER resizing when you can: the same visual softness needs a much smaller sigma on a smaller image.",
            "The alpha channel is blurred too.",
                    MASK_WARNING,
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
        },
        MASK_PARAM,
        ],
        examples: &[
            r#"{"op": "median", "radius": 2}"#,
            r#"{"op": "median", "radius": 5}"#,
        ],
        warnings: &[
            "Cost is O(radius^2) per pixel; radius above ~5 is slow on large images.",
            "It removes fine texture along with the noise. Preview before committing.",
                    MASK_WARNING,
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
        MASK_PARAM,
        ],
        examples: &[
            r#"{"op": "unsharp_mask", "amount": 0.8, "radius": 1.2, "threshold": 3}"#,
            r#"{"op": "unsharp_mask", "amount": 1.5, "radius": 2.0}"#,
        ],
        warnings: &[
            "threshold compares per-channel differences, not luminance (a deliberate simplification for determinism).",
            "Sharpen LAST, after the final resize; sharpening before downscaling produces halos.",
                    MASK_WARNING,
        ],
    },
    OpDoc {
        name: "convolve",
        category: "filter",
        summary: "Arbitrary NxN convolution kernel (edges, emboss, sharpen).",
        params: &[
            ParamDoc {
                name: "kernel",
                type_hint: "f64[size*size]",
                requirement: "required",
                semantics: "Row-major kernel weights; exactly size*size entries, each finite with |v| <= 256.",
            },
            ParamDoc {
                name: "size",
                type_hint: "u32 3|5|7|9",
                requirement: "required",
                semantics: "Kernel edge length; only odd sizes 3, 5, 7 and 9 are allowed.",
            },
            ParamDoc {
                name: "divisor",
                type_hint: "f64 |v|>=1e-6",
                requirement: "default: 1.0",
                semantics: "The weighted sum is divided by this before offset is added; use the sum of the kernel to keep brightness.",
            },
            ParamDoc {
                name: "offset",
                type_hint: "f64 -255..255",
                requirement: "default: 0",
                semantics: "Constant added after the division, in 0..255 units; typically 128 for emboss/edge kernels that center on zero.",
            },
        MASK_PARAM,
        ],
        examples: &[
            r#"{"op": "convolve", "kernel": [0,-1,0, -1,5,-1, 0,-1,0], "size": 3}"#,
            r#"{"op": "convolve", "kernel": [-2,-1,0, -1,1,1, 0,1,2], "size": 3, "offset": 128}"#,
        ],
        warnings: &[
            "kernel.len() must equal size*size and divisor must not be ~0; both are validation errors.",
            "Only RGB is convolved; the alpha channel passes through untouched. Borders replicate the edge pixel.",
            "For plain sharpening prefer unsharp_mask (radius/threshold control); convolve is the escape hatch for W3C feConvolveMatrix-style effects.",
                    MASK_WARNING,
        ],
    },
    OpDoc {
        name: "clone",
        category: "filter",
        summary: "Stamp a circular patch of pixels from one point onto another (Photoshop clone stamp).",
        params: &[
            ParamDoc {
                name: "src_x",
                type_hint: "u32",
                requirement: "required",
                semantics: "X of the source point pixels are copied FROM, in the current image's coordinates.",
            },
            ParamDoc {
                name: "src_y",
                type_hint: "u32",
                requirement: "required",
                semantics: "Y of the source point pixels are copied FROM.",
            },
            ParamDoc {
                name: "dest_x",
                type_hint: "u32",
                requirement: "required",
                semantics: "X of the destination point pixels are copied TO.",
            },
            ParamDoc {
                name: "dest_y",
                type_hint: "u32",
                requirement: "required",
                semantics: "Y of the destination point pixels are copied TO.",
            },
            ParamDoc {
                name: "radius",
                type_hint: "u32 1..2048",
                requirement: "required",
                semantics: "Radius in pixels of the circular patch copied from src to dest; must be within 1..=2048.",
            },
            ParamDoc {
                name: "feather_px",
                type_hint: "f64 0..200",
                requirement: "default: 0",
                semantics: "Softens the patch edge over this many pixels (gaussian-like falloff, 0..=200) so the stamp blends instead of leaving a hard disc outline. Omitted or 0 means a hard edge.",
            },
        ],
        examples: &[
            r#"{"op": "clone", "src_x": 900, "src_y": 400, "dest_x": 1200, "dest_y": 620, "radius": 40}"#,
            r#"{"op": "clone", "src_x": 900, "src_y": 400, "dest_x": 1200, "dest_y": 620, "radius": 40, "feather_px": 8.0}"#,
        ],
        warnings: &[
            "This is a straight pixel copy (source texture AND tone both move to dest); for a dust-spot/blemish fix where the surrounding tone must be preserved, use heal instead.",
            "The source and destination circles are both clamped to the image bounds; a source circle that falls partly outside the image only copies the part that exists.",
            "The seed/algorithm is fixed (no randomness), so the same recipe always reproduces the same pixels.",
        ],
    },
    OpDoc {
        name: "heal",
        category: "filter",
        summary: "Copy texture from src to dest while matching the surrounding tone at dest (Photoshop healing brush).",
        params: &[
            ParamDoc {
                name: "src_x",
                type_hint: "u32",
                requirement: "required",
                semantics: "X of the source point texture is sampled FROM, in the current image's coordinates.",
            },
            ParamDoc {
                name: "src_y",
                type_hint: "u32",
                requirement: "required",
                semantics: "Y of the source point texture is sampled FROM.",
            },
            ParamDoc {
                name: "dest_x",
                type_hint: "u32",
                requirement: "required",
                semantics: "X of the destination point being healed.",
            },
            ParamDoc {
                name: "dest_y",
                type_hint: "u32",
                requirement: "required",
                semantics: "Y of the destination point being healed.",
            },
            ParamDoc {
                name: "radius",
                type_hint: "u32 1..2048",
                requirement: "required",
                semantics: "Radius in pixels of the circular patch healed at dest; must be within 1..=2048.",
            },
            ParamDoc {
                name: "feather_px",
                type_hint: "f64 0..200",
                requirement: "default: 0",
                semantics: "Softens the patch edge over this many pixels (0..=200) so the healed disc blends into its surroundings. Omitted or 0 means a hard edge.",
            },
        ],
        examples: &[
            r#"{"op": "heal", "src_x": 300, "src_y": 150, "dest_x": 620, "dest_y": 210, "radius": 18}"#,
            r#"{"op": "heal", "src_x": 300, "src_y": 150, "dest_x": 620, "dest_y": 210, "radius": 18, "feather_px": 4.0}"#,
        ],
        warnings: &[
            "copies texture from src while adopting the tone around dest (texture+tone split) - this is what makes it the right op for blemishes/dust spots on a background whose brightness varies, where plain clone would paste a visibly mismatched patch.",
            "The source and destination circles are both clamped to the image bounds.",
            "The seed/algorithm is fixed (no randomness), so the same recipe always reproduces the same pixels.",
        ],
    },
    OpDoc {
        name: "svg_overlay",
        // 分類は "filter": 語彙上は「画素をその場で書き換える op」であって、
        // キャンバスの寸法や座標系を動かす geometry でも、出力形式を決める output でもない。
        category: "filter",
        summary: "Stamp an imported SVG (logo/watermark/credit) onto the image at a position.",
        params: &[
            ParamDoc {
                name: "svg_revision_id",
                type_hint: "\"rev_...\"",
                requirement: "required",
                semantics: "Revision id of an SVG already imported into THIS workspace. Workflow: call import_asset on the .svg file first (it becomes a revision with mime_type image/svg+xml), then put the returned revision_id here.",
            },
            ParamDoc {
                name: "x",
                type_hint: "i64",
                requirement: "required",
                semantics: "X of the overlay's TOP-LEFT corner, in the current image's pixel coordinates. May be negative: the part that falls outside the image is clipped, not an error.",
            },
            ParamDoc {
                name: "y",
                type_hint: "i64",
                requirement: "required",
                semantics: "Y of the overlay's top-left corner. May be negative (clipped).",
            },
            ParamDoc {
                name: "width",
                type_hint: "u32 1..32768",
                requirement: "optional",
                semantics: "Width to rasterize the SVG at. Omit both width and height to use the SVG's own intrinsic size; give only one and the other is derived from the intrinsic aspect ratio; give both to stretch to an exact box.",
            },
            ParamDoc {
                name: "height",
                type_hint: "u32 1..32768",
                requirement: "optional",
                semantics: "Height to rasterize the SVG at. See width for how the two combine.",
            },
            ParamDoc {
                name: "opacity",
                type_hint: "f64 0..1",
                requirement: "default: 1.0",
                semantics: "Overall strength of the overlay, multiplied into the SVG's own alpha. 0 leaves the image byte-identical, 1 stamps at full strength (a typical watermark is 0.15-0.4).",
            },
            ParamDoc {
                name: "blend_mode",
                type_hint: "enum, 16 modes",
                requirement: "default: normal",
                semantics: "The same 16 W3C blend modes as layers: normal | multiply | screen | overlay | darken | lighten | color_dodge | color_burn | hard_light | soft_light | difference | exclusion | hue | saturation | color | luminosity. Compositing uses the identical W3C formula, so a watermark behaves exactly as it would as a layer.",
            },
        ],
        examples: &[
            r#"{"op": "svg_overlay", "svg_revision_id": "rev_01J000000000000000000000", "x": 24, "y": 24}"#,
            r#"{"op": "svg_overlay", "svg_revision_id": "rev_01J000000000000000000000", "x": -40, "y": 900, "width": 320, "opacity": 0.25, "blend_mode": "screen"}"#,
        ],
        warnings: &[
            "TEXT IS NOT RENDERED. atx never loads system fonts, because the installed fonts differ from machine to machine and would break byte-for-byte reproducibility. An SVG containing <text> renders its shapes but not its glyphs, and reports a warning. Convert text to paths (outlines) in your vector editor before importing.",
            "The SVG must be in the workspace: import_asset the .svg file FIRST, then reference the revision_id it returns. A recipe pointing at an unknown id fails with a structured error before any pixel work happens.",
            "Each of width/height is limited to 1..=32768, and the rasterized overlay may not exceed 100,000,000 pixels in total (width * height) - a bigger request is a structured error at run time, not a silent downscale.",
            "An SVG with no intrinsic size (no viewBox and no absolute width/height on the root <svg>) is a structured error unless you give BOTH width and height - the engine will not silently fall back to a default size. import_asset reports the intrinsic size (0x0 means it has none).",
            "inspect_image refuses an SVG revision on purpose - it is a vector asset, not a raster image. So do apply_transform/render_preview when pointed AT the SVG: the SVG is what you reference from a recipe, not what you transform.",
            "x/y are the TOP-LEFT corner, not the centre, and are interpreted in the image as it is at THIS point of the pipeline - put the overlay after your resize/crop, or the watermark lands in the wrong place and at the wrong scale.",
            "The recipe hash includes the referenced revision id, so a recipe is only reproducible inside a workspace that holds that SVG. Export/import the .svg alongside the recipe to move a watermark between machines.",
        ],
    },
    OpDoc {
        name: "flip",
        category: "geometry",
        summary: "Mirror the image horizontally or vertically.",
        params: &[ParamDoc {
            name: "direction",
            type_hint: "enum horizontal|vertical",
            requirement: "required",
            semantics: "horizontal mirrors left-right (a horizontal flip about the vertical axis); vertical mirrors top-bottom.",
        }],
        examples: &[
            r#"{"op": "flip", "direction": "horizontal"}"#,
            r#"{"op": "flip", "direction": "vertical"}"#,
        ],
        warnings: &[
            "A pure pixel remap: dimensions are unchanged and nothing is cropped or padded.",
            "No mask param - flip moves every pixel's position, so a per-pixel weight from the pre-flip layout would not line up with the result.",
            "Flipping an image that contains readable text or a watermark mirrors it too; svg_overlay/text is not aware of a prior flip and must be positioned after it.",
        ],
    },
    OpDoc {
        name: "vignette",
        category: "filter",
        summary: "Darken (or lighten) the image towards the edges.",
        params: &[
            ParamDoc {
                name: "strength",
                type_hint: "f64 -1..1",
                requirement: "required",
                semantics: "Amount of exposure change at the edge; positive darkens (classic vignette), negative lightens. 0 is a no-op.",
            },
            ParamDoc {
                name: "radius",
                type_hint: "f64 0..1.5",
                requirement: "default: 0.7",
                semantics: "Normalized distance from center (as a fraction of the half-diagonal) where the falloff begins; below this radius the image is untouched. 0 starts the falloff at the center, 1.5 pushes it past the corners.",
            },
            ParamDoc {
                name: "feather",
                type_hint: "f64 0..1",
                requirement: "default: 0.5",
                semantics: "Width of the falloff band (same normalized units as radius) over which the effect ramps from 0 to full strength.",
            },
        ],
        examples: &[
            r#"{"op": "vignette", "strength": 0.35}"#,
            r#"{"op": "vignette", "strength": 0.5, "radius": 0.55, "feather": 0.35}"#,
        ],
        warnings: &[
            "Applied in LINEAR light as an exposure falloff, so the same strength reads darker on a bright, high-key image than on an already-dark one.",
            "No mask param - vignette is already a radial, centered effect; use a low radius/high feather for a tight look instead of trying to mask it into a shape.",
            "vignette never changes the canvas size (unlike crop): it only darkens/lightens existing pixels, nothing is trimmed.",
            "radius + feather extending past 1.5/1.0 respectively can leave the effect invisible if it never reaches the visible area; preview before committing.",
        ],
    },
    OpDoc {
        name: "grain",
        category: "filter",
        summary: "Add film-style grain/noise.",
        params: &[
            ParamDoc {
                name: "amount",
                type_hint: "f64 0..1",
                requirement: "required",
                semantics: "Grain intensity; 0 leaves the image unchanged, 1 is heavy.",
            },
            ParamDoc {
                name: "size",
                type_hint: "u32 1..4",
                requirement: "default: 1",
                semantics: "Grain particle size in pixels (coarseness); 1 is fine single-pixel grain, 4 is chunky.",
            },
            ParamDoc {
                name: "monochrome",
                type_hint: "bool",
                requirement: "default: true",
                semantics: "true modulates luminance only (classic film grain, no color speckle); false modulates each channel independently (colored noise).",
            },
            ParamDoc {
                name: "seed",
                type_hint: "u64",
                requirement: "default: 0",
                semantics: "Seed for the deterministic noise pattern. Same seed + same image = pixel-identical grain every run.",
            },
        MASK_PARAM,
        ],
        examples: &[
            r#"{"op": "grain", "amount": 0.15}"#,
            r#"{"op": "grain", "amount": 0.3, "size": 2, "monochrome": false, "seed": 7}"#,
        ],
        warnings: &[
            "Deterministic and reproducible: it is a coordinate+seed hash, not a random-crate RNG, so the same recipe always produces the same grain. Change seed to get a different-looking pattern on purpose.",
            "Applied in sRGB code-value space (perceptual), not linear light, matching how film grain is normally judged by eye.",
            "Grain survives resizing awkwardly: apply it AFTER your final resize, or the grain gets scaled/blurred along with the image.",
                    MASK_WARNING,
        ],
    },
    OpDoc {
        name: "gradient_map",
        category: "color",
        summary: "Map luminance to a color gradient (duotone/tritone looks).",
        params: &[
            ParamDoc {
                name: "stops",
                type_hint: "[{position,color}] 2..8",
                requirement: "required",
                semantics: "2 to 8 stops, each {position: f64 0..1, color: css hex}, position strictly increasing. Each pixel's BT.709 luminance (0=black..1=white) is looked up in this gradient and linearly interpolated between the two nearest stops; outside the first/last stop the end color is clamped.",
            },
        MASK_PARAM,
        ],
        examples: &[
            r##"{"op": "gradient_map", "stops": [{"position": 0.0, "color": "#0a1f3d"}, {"position": 1.0, "color": "#f5ead0"}]}"##,
            r##"{"op": "gradient_map", "stops": [{"position": 0.0, "color": "#1a0d2e"}, {"position": 0.5, "color": "#a3325f"}, {"position": 1.0, "color": "#ffd27f"}]}"##,
        ],
        warnings: &[
            "8-digit hex (#rrggbbaa) is rejected on purpose: stops are opaque colors and a silently-ignored alpha would be a surprise. Use 3/4/6-digit hex; pixel alpha is preserved unchanged separately.",
            "This REPLACES color entirely - original hue/saturation are discarded, only luminance survives as the lookup key. Run adjust/curves first if you want to shape contrast going into the map.",
            "Applied in sRGB code-value space; two very different colors of the same code-value luminance map to the same gradient color.",
                    MASK_WARNING,
        ],
    },
    OpDoc {
        name: "pixelate",
        category: "filter",
        summary: "Mosaic/pixelate blocks of the image (privacy redaction staple).",
        params: &[
            ParamDoc {
                name: "block_size",
                type_hint: "u32 2..256",
                requirement: "required",
                semantics: "Edge length in pixels of each averaged square block. Larger = coarser/more anonymous.",
            },
            ParamDoc {
                name: "region",
                type_hint: "{x,y,width,height} u32",
                requirement: "optional",
                semantics: "Pixel rect to pixelate, in the CURRENT image's coordinates. Omitted pixelates the whole image. The block grid is anchored at the region's top-left corner so a re-run with the same region looks stable.",
            },
        ],
        examples: &[
            r#"{"op": "pixelate", "block_size": 24}"#,
            r#"{"op": "pixelate", "block_size": 18, "region": {"x": 400, "y": 220, "width": 260, "height": 260}}"#,
        ],
        warnings: &[
            "The privacy staple for faces/license plates: pick a block_size large enough that no recognizable detail survives (as a rule of thumb, big enough that a face spans well under ~4 blocks across).",
            "Block averaging is done in LINEAR light, not sRGB code values, so an averaged block's apparent brightness is correct even across a hard light/dark edge - a naive sRGB average would look too dark.",
            "No mask param - region (a plain rect) already defines the applied area, and pixelate is typically used for hard privacy redaction where a soft/feathered edge would defeat the purpose.",
            "region is in the CURRENT image's coordinates: place pixelate after any resize/crop that changes where your subject sits, or the redacted area lands in the wrong place.",
        ],
    },
    OpDoc {
        name: "auto_levels",
        category: "color",
        summary: "Automatic black/white point stretch from the image's own histogram.",
        params: &[
            ParamDoc {
                name: "clip_percent",
                type_hint: "f64 0..10",
                requirement: "default: 0.5",
                semantics: "Percent of the histogram clipped at each end before stretching the remainder to 0..1 (per-mille-ish tolerance for outlier pixels). 0 stretches to the true min/max.",
            },
            ParamDoc {
                name: "per_channel",
                type_hint: "bool",
                requirement: "default: false",
                semantics: "false computes one stretch from the BT.709 luminance histogram and applies it to all channels equally (preserves color balance). true computes an independent stretch per R/G/B channel (corrects a color cast, e.g. an underwater or tungsten-lit shot).",
            },
        MASK_PARAM,
        ],
        examples: &[
            r#"{"op": "auto_levels"}"#,
            r#"{"op": "auto_levels", "clip_percent": 1.0, "per_channel": true}"#,
        ],
        warnings: &[
            "per_channel=true SHIFTS HUES: stretching R/G/B independently is exactly what fixes a color cast, but on a scene with no cast it can introduce one. Try per_channel=false first, and only switch on a genuine tungsten/underwater-style tint.",
            "Deterministic for a given input image: the histogram is a fixed-bin integer count, so the same image always yields the same stretch (no randomness), but the stretch itself depends on the image's own content - it is not a fixed curve like levels.",
            "clip_percent too high on a small or low-contrast image can clip meaningful highlight/shadow detail along with outliers; start near the default 0.5 and inspect the result.",
                    MASK_WARNING,
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
            ParamDoc {
                name: "bit_depth",
                type_hint: "u8 8|16",
                requirement: "default: 8",
                semantics: "Output bit depth. 16 is valid for png only (preserves the f32 engine's precision).",
            },
        ],
        examples: &[
            r#"{"op": "encode", "format": "webp", "quality": 82}"#,
            r#"{"op": "encode", "format": "png", "bit_depth": 16}"#,
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

/// `layers`(recipe-structure、v0.6)の擬似エントリ。
///
/// これは op ではない: `{"op": "..."}` の名前ではなく、レシピ自体の形
/// (`{"layers": [...]}`)を説明するリファレンスである。したがって
/// **[`OPERATIONS`] には含めない**(`list_operations` のカタログは op のみを
/// 保つ、`unknown_operation` エラーの `valid_values` にも出さない — instructions の
/// 一文と、この `explain_operation("layers")` から発見できれば十分)。
/// `tools::explain_operation` が [`find`] より先にこの名前を特別扱いする。
pub const LAYERS_DOC: OpDoc = OpDoc {
    name: "layers",
    category: "structure",
    summary: "Recipe structure (not an op): a bottom-to-top layer stack composited before the top-level operations run as a finishing pass.",
    params: &[
        ParamDoc {
            name: "layers[].source",
            type_hint: "\"base\" | {revision_id}",
            requirement: "required",
            semantics: "\"base\" means the input revision passed to apply_transform/render_preview. {\"revision_id\": \"rev_...\"} references any other revision already in this workspace. A source of any size is accepted: what must match is the layer's size AFTER its own ops (see layers[].ops), not the size of the source revision itself.",
        },
        ParamDoc {
            name: "layers[].ops",
            type_hint: "Operation[]",
            requirement: "default: []",
            semantics: "A normal operations list applied to this layer's own source before it is composited; omit it (or pass []) to use the source unchanged. Any op is allowed EXCEPT encode and strip_metadata, which are finishing-pass-only and rejected inside a layer. This is also where you put the resize/crop that brings the layer to the backdrop's dimensions.",
        },
        ParamDoc {
            name: "layers[].mask",
            type_hint: "{revision_id,invert,feather_px}",
            requirement: "optional",
            semantics: "Same shape and semantics as the per-op mask (white = this layer contributes fully at that pixel, black = the backdrop shows through unchanged there), applied at composite time on top of blend_mode/opacity. Rejected on layers[0]: the backdrop has nothing underneath it to show through.",
        },
        ParamDoc {
            name: "layers[].blend_mode",
            type_hint: "enum, 16 modes (12 separable + 4 non-separable)",
            requirement: "default: normal",
            semantics: "One of normal | multiply | screen | overlay | darken | lighten | color_dodge | color_burn | hard_light | soft_light | difference | exclusion (the W3C separable blend modes, applied per-channel) or hue | saturation | color | luminosity (the W3C non-separable blend modes, applied to the HSL components of the composite as a whole rather than per-channel). On layers[0] only \"normal\" is accepted (see the backdrop warning below).",
        },
        ParamDoc {
            name: "layers[].opacity",
            type_hint: "f64 0..1",
            requirement: "default: 1.0",
            semantics: "Overall strength of this layer's contribution after blend_mode, linearly blended with the backdrop. 0 = invisible, 1 = full strength. On layers[0] only 1.0 is accepted (see the backdrop warning below).",
        },
    ],
    examples: &[
        r#"{"layers": [{"source": "base", "ops": []}, {"source": {"revision_id": "rev_01J000000000000000000000"}, "ops": [{"op": "blur", "sigma": 8}], "blend_mode": "multiply", "opacity": 0.6}], "operations": [{"op": "resize", "width": 1600}, {"op": "encode", "format": "webp", "quality": 82}]}"#,
    ],
    warnings: &[
        "layers is a top-level recipe field, not something you put inside \"operations\" - a recipe has EITHER a flat operations pipeline OR a layers stack (with operations as its finishing pass), never a \"layers\" op tag.",
        "layers[0] IS THE BACKDROP and there is nothing underneath it to blend with, so it is REJECTED unless blend_mode is \"normal\", opacity is exactly 1.0, and it carries no mask. Put the look you wanted on layers[0] into a layer above it instead.",
        "DIMENSIONS ARE CHECKED AFTER EACH LAYER'S OWN ops, against the backdrop (layers[0] after its ops) - not against the base image, and not against the raw source revision. So a layer may start from a source of any size: the fix for a mismatch is to add a resize/crop to THAT layer's ops until it matches the backdrop, and the error message tells you the exact target size.",
        "layers[].ops is optional (an omitted ops is the same as []), but encode and strip_metadata are rejected inside a layer - they belong to the top-level finishing pass.",
        "When layers is present, the top-level \"operations\" list runs ONCE on the composited result as the finishing pass - this is where resize and the final encode belong. encode must still be the last operation and appear at most once.",
        "16 blend modes total: the 12 W3C separable modes (normal, multiply, screen, overlay, darken, lighten, color_dodge, color_burn, hard_light, soft_light, difference, exclusion) plus the 4 non-separable modes (hue, saturation, color, luminosity) added in v0.7.",
        "recipe_hash covers the whole recipe including every layer's ops and any referenced revision ids (same rule as lut/mask references), so a layered recipe reproduces only inside a workspace holding every referenced revision.",
    ],
};

/// 名前から op を引く。`layers`(構造リファレンス)はここには含まれない -
/// `tools::explain_operation` が別経路で扱う。
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
        assert_eq!(names.len(), 27, "v0.3.0 has 27 operations");
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

    /// `clone` / `heal` / `svg_overlay` の examples も
    /// `atx_core::recipe::Operation` としてデシリアライズできること
    /// (v0.7 / v0.8 core 着地済み)。
    #[test]
    fn clone_heal_examples_deserialize_as_operations() {
        for op in OPERATIONS
            .iter()
            .filter(|op| op.name == "clone" || op.name == "heal" || op.name == "svg_overlay")
        {
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
