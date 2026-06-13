//! HID report descriptor primitives.
//!
//! Three layers, lowest to highest:
//!
//! * [`Items`] / [`RawItem`], a zero-copy lexer over the short/long items of
//!   a raw descriptor.
//! * [`ReportDescriptor`], a parsed model: the collection tree plus every
//!   declared report with field offsets, usages, logical ranges and sizes.
//! * [`DescriptorBuilder`], an encoder that emits descriptor bytes, used by
//!   the WebHID backend to reconstruct descriptors from browser-parsed
//!   collection data and usable standalone.
//!
//! These primitives are platform-independent and compiled on every target.

mod build;
mod items;
mod parse;

pub use build::DescriptorBuilder;
pub use items::{GlobalTag, ItemType, Items, LocalTag, MainTag, RawItem};
pub use parse::{
    Collection, CollectionKind, Field, MainFlags, Report, ReportDescriptor, ReportKind, Usage,
};
