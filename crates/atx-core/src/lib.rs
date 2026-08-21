//! atx-core: レシピ型定義・正規化・ハッシュ・決定論的変換エンジン。
//! MCP 非依存。CLI やテストから直接利用できる。

mod codec;
pub mod engine;
pub(crate) mod ops;
mod pixel_ops;
pub mod recipe;
pub(crate) mod transform;

pub use engine::{apply_recipe, inspect_bytes, EncodedOutput, ImageInfo, ENGINE_VERSION};
pub use recipe::{
    canonical_json, recipe_hash, Anchor, CoordinateSpace, CropMode, Fit, Operation, OutputFormat,
    RotateCrop, StripScope, TransformRecipe,
};

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
