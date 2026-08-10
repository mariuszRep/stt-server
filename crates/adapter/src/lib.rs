mod traits;
mod error;

pub mod mock;
#[cfg(feature = "whisper")]
pub mod whisper_adapter;

pub use traits::*;
pub use error::*;
