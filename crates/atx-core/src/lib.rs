//! atx-core: レシピ型定義・正規化・ハッシュ・決定論的変換エンジン。
//! MCP 非依存。CLI やテストから直接利用できる。

mod codec;
pub mod engine;
pub(crate) mod linear;
pub(crate) mod ops;
pub(crate) mod parallel;
mod pixel_ops;
pub mod recipe;
pub mod similarity;
pub mod stats;
pub(crate) mod transform;

pub use engine::{
    apply_recipe, apply_recipe_with_assets, decode_oriented, inspect_bytes, inspect_bytes_with,
    read_exif_all, AssetResolver, EncodedOutput, ExifEntry, ImageInfo, NoAssets, ENGINE_VERSION,
};
/// .cube LUT アセットの取り込み時検証。`import_asset` が使う。
pub use ops::lut::validate_asset as validate_cube_asset;
/// SVG バイト列の**固有サイズ**(px)。固有サイズを持たない/パースできない SVG は `None`。
///
/// `import_asset` が台帳へ寸法を記録するための入口。これを core 側に生やすことで、
/// atx-mcp / atx-store は resvg へ直接依存しなくて済む(依存はラスタライザを持つ
/// atx-core 1 箇所に閉じる)。
pub use ops::svg::intrinsic_size as svg_intrinsic_size;
/// SVG アセットの取り込み時検証(成功なら固有サイズ)。`import_asset` が使う。
pub use ops::svg::validate_asset as validate_svg_asset;
/// font アセット(.ttf / .otf)の検証と、そのバイト上限。`import_asset` が使う。
///
/// `validate_font_asset` は SVG / LUT と同じ規約で、失敗を**英語の平文**
/// (`Err(String)`)で返す(MCP 層がそのまま利用者へ見せる)。成功時の
/// `FontAssetInfo.families` は SVG の `font-family` に書ける名前。
pub use ops::svg::{validate_font_asset, FontAssetInfo, MAX_FONT_BYTES};
pub use recipe::{
    canonical_json, recipe_hash, Anchor, BaseKeyword, BlendMode, CoordinateSpace, CropMode, Fit,
    Layer, LayerSource, MaskRef, Operation, OutputFormat, RotateCrop, StripScope, TransformRecipe,
};
/// 知覚ハッシュ(dHash)と SSIM。`inspect_image` / `compare_revisions` が使う。
pub use similarity::{dhash_hex, dhash_rgb8, dhash_rgba8, hamming, ssim_gray};
pub use stats::ImageStats;

/// atx-core 全体のエラー型。op 単位の失敗位置を保持し、LLM が自己修復できる粒度で返す。
#[derive(Debug, thiserror::Error)]
pub enum AtxError {
    #[error("failed to decode input image: {0}")]
    Decode(String),
    #[error("failed to encode output image: {0}")]
    Encode(String),
    #[error("invalid recipe: {0}")]
    InvalidRecipe(String),
    #[error("operation {index} ({op}) failed: {message}")]
    Operation {
        index: usize,
        op: String,
        message: String,
    },
    #[error("input exceeds limits: {0}")]
    LimitExceeded(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, AtxError>;

/// 入力ガード上限。
pub struct Limits {
    /// 最大画素数(幅×高さ)。デフォルト 100MP。
    pub max_pixels: u64,
    /// 最大入力バイトサイズ。デフォルト 128MiB。
    pub max_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_pixels: 100_000_000,
            max_bytes: 128 * 1024 * 1024,
        }
    }
}
