//! Geometry-based regions and mixed-style rewriting of existing text.

use super::*;

#[path = "text_regions/grouping.rs"]
mod grouping;
#[path = "text_regions/styled.rs"]
mod styled;

#[cfg(test)]
pub(super) use grouping::group_for_test;
pub(super) use grouping::{group_blocks, group_reading_blocks, is_centred, ruled_apart};
pub use grouping::{line_regions, set_line_regions};
pub(super) use styled::rewrite_styled_block;
