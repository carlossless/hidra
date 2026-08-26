//! Report descriptor encoder.
//!
//! [`DescriptorBuilder`] emits short items with minimally-sized payloads.
//! The `WebHID` backend uses it to reconstruct a byte-level report descriptor
//! from the parsed collection data the browser exposes; it is also useful on
//! its own for building descriptors in tests, emulated devices or firmware
//! tooling.

use super::items::{GlobalTag, ItemType, LocalTag, MainTag};
use super::parse::{CollectionKind, MainFlags};

/// Builds a raw HID report descriptor item by item.
///
/// ```
/// use hidra::descriptor::{DescriptorBuilder, MainFlags, ReportDescriptor, ReportKind};
///
/// let mut b = DescriptorBuilder::new();
/// b.usage_page(0xFF00) // vendor
///     .usage(0x01)
///     .collection(hidra::descriptor::CollectionKind::Application)
///     .logical_minimum(0)
///     .logical_maximum(255)
///     .report_size(8)
///     .report_count(64)
///     .usage(0x02)
///     .input(MainFlags::VARIABLE)
///     .end_collection();
/// let bytes = b.build();
///
/// let parsed = ReportDescriptor::parse(&bytes).unwrap();
/// assert_eq!(parsed.max_report_size(ReportKind::Input), 64);
/// ```
#[derive(Debug, Clone, Default)]
pub struct DescriptorBuilder {
    bytes: Vec<u8>,
}

impl DescriptorBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a short item with an unsigned payload, using the smallest
    /// encoding that holds `value`.
    pub fn item_unsigned(&mut self, item_type: ItemType, tag: u8, value: u32) -> &mut Self {
        let bytes = value.to_le_bytes();
        let len = match value {
            0 => 0,
            0x1..=0xFF => 1,
            0x100..=0xFFFF => 2,
            _ => 4,
        };
        self.push_item(item_type, tag, &bytes[..len])
    }

    /// Append a short item with a signed payload, using the smallest
    /// encoding that sign-extends back to `value`.
    pub fn item_signed(&mut self, item_type: ItemType, tag: u8, value: i32) -> &mut Self {
        let bytes = value.to_le_bytes();
        let len = if value == 0 {
            0
        } else if i8::try_from(value).is_ok() {
            1
        } else if i16::try_from(value).is_ok() {
            2
        } else {
            4
        };
        self.push_item(item_type, tag, &bytes[..len])
    }

    fn push_item(&mut self, item_type: ItemType, tag: u8, data: &[u8]) -> &mut Self {
        debug_assert!(matches!(data.len(), 0 | 1 | 2 | 4));
        debug_assert!(tag <= 0xF);
        let size_code = match data.len() {
            4 => 3,
            n => n as u8,
        };
        self.bytes
            .push((tag << 4) | (item_type.code() << 2) | size_code);
        self.bytes.extend_from_slice(data);
        self
    }

    // --- Global items -----------------------------------------------------

    /// Emit a `Usage Page` global item.
    pub fn usage_page(&mut self, page: u16) -> &mut Self {
        self.item_unsigned(ItemType::Global, GlobalTag::UsagePage.code(), page as u32)
    }

    /// Emit a `Logical Minimum` global item.
    pub fn logical_minimum(&mut self, value: i32) -> &mut Self {
        self.item_signed(ItemType::Global, GlobalTag::LogicalMinimum.code(), value)
    }

    /// Emit a `Logical Maximum` global item.
    pub fn logical_maximum(&mut self, value: i32) -> &mut Self {
        self.item_signed(ItemType::Global, GlobalTag::LogicalMaximum.code(), value)
    }

    /// Emit a `Physical Minimum` global item.
    pub fn physical_minimum(&mut self, value: i32) -> &mut Self {
        self.item_signed(ItemType::Global, GlobalTag::PhysicalMinimum.code(), value)
    }

    /// Emit a `Physical Maximum` global item.
    pub fn physical_maximum(&mut self, value: i32) -> &mut Self {
        self.item_signed(ItemType::Global, GlobalTag::PhysicalMaximum.code(), value)
    }

    /// Emit a `Unit Exponent` global item.
    pub fn unit_exponent(&mut self, value: i32) -> &mut Self {
        self.item_signed(ItemType::Global, GlobalTag::UnitExponent.code(), value)
    }

    /// Emit a `Unit` global item.
    pub fn unit(&mut self, value: u32) -> &mut Self {
        self.item_unsigned(ItemType::Global, GlobalTag::Unit.code(), value)
    }

    /// Emit a `Report Size` global item.
    pub fn report_size(&mut self, bits: u32) -> &mut Self {
        self.item_unsigned(ItemType::Global, GlobalTag::ReportSize.code(), bits)
    }

    /// Emit a `Report ID` global item.
    pub fn report_id(&mut self, id: u8) -> &mut Self {
        self.item_unsigned(ItemType::Global, GlobalTag::ReportId.code(), id as u32)
    }

    /// Emit a `Report Count` global item.
    pub fn report_count(&mut self, count: u32) -> &mut Self {
        self.item_unsigned(ItemType::Global, GlobalTag::ReportCount.code(), count)
    }

    /// Emit a `Push` global item.
    pub fn push(&mut self) -> &mut Self {
        self.push_item(ItemType::Global, GlobalTag::Push.code(), &[])
    }

    /// Emit a `Pop` global item.
    pub fn pop(&mut self) -> &mut Self {
        self.push_item(ItemType::Global, GlobalTag::Pop.code(), &[])
    }

    // --- Local items -------------------------------------------------------

    /// Emit a `Usage` item. Values above `0xFFFF` are emitted as 4-byte
    /// extended usages (page in the high half).
    pub fn usage(&mut self, usage: u32) -> &mut Self {
        self.item_unsigned(ItemType::Local, LocalTag::Usage.code(), usage)
    }

    /// Emit a `Usage Minimum` local item.
    pub fn usage_minimum(&mut self, usage: u32) -> &mut Self {
        self.item_unsigned(ItemType::Local, LocalTag::UsageMinimum.code(), usage)
    }

    /// Emit a `Usage Maximum` local item.
    pub fn usage_maximum(&mut self, usage: u32) -> &mut Self {
        self.item_unsigned(ItemType::Local, LocalTag::UsageMaximum.code(), usage)
    }

    /// Emit a `String Index` local item.
    pub fn string_index(&mut self, index: u32) -> &mut Self {
        self.item_unsigned(ItemType::Local, LocalTag::StringIndex.code(), index)
    }

    // --- Main items ----------------------------------------------------------

    /// Emit a `Collection` main item of the given kind.
    pub fn collection(&mut self, kind: CollectionKind) -> &mut Self {
        self.item_unsigned(
            ItemType::Main,
            MainTag::Collection.code(),
            kind.value() as u32,
        )
    }

    /// Emit an `End Collection` main item.
    pub fn end_collection(&mut self) -> &mut Self {
        self.push_item(ItemType::Main, MainTag::EndCollection.code(), &[])
    }

    /// Emit an `Input` main item.
    pub fn input(&mut self, flags: MainFlags) -> &mut Self {
        self.main(MainTag::Input, flags)
    }

    /// Emit an `Output` main item.
    pub fn output(&mut self, flags: MainFlags) -> &mut Self {
        self.main(MainTag::Output, flags)
    }

    /// Emit a `Feature` main item.
    pub fn feature(&mut self, flags: MainFlags) -> &mut Self {
        self.main(MainTag::Feature, flags)
    }

    fn main(&mut self, tag: MainTag, flags: MainFlags) -> &mut Self {
        // Input/Output/Feature items conventionally carry at least one data
        // byte even when all flags are zero.
        if flags == MainFlags::NONE {
            self.push_item(ItemType::Main, tag.code(), &[0])
        } else {
            self.item_unsigned(ItemType::Main, tag.code(), flags.bits())
        }
    }

    /// Finish and return the descriptor bytes.
    pub fn build(self) -> Vec<u8> {
        self.bytes
    }

    /// The bytes emitted so far.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse::{MainFlags, ReportDescriptor, ReportKind, Usage};
    use super::*;

    #[test]
    fn builder_output_parses_back() {
        let mut b = DescriptorBuilder::new();
        b.usage_page(0x01)
            .usage(0x02)
            .collection(CollectionKind::Application)
            .report_id(5)
            .logical_minimum(-127)
            .logical_maximum(127)
            .report_size(8)
            .report_count(3)
            .usage(0x30)
            .usage(0x31)
            .usage(0x38)
            .input(MainFlags::VARIABLE | MainFlags::RELATIVE)
            .end_collection();
        let bytes = b.build();

        let parsed = ReportDescriptor::parse(&bytes).unwrap();
        let report = parsed.report(ReportKind::Input, Some(5)).unwrap();
        assert_eq!(report.size_bytes(), 3);
        let field = &report.fields[0];
        assert_eq!(field.logical_minimum, -127);
        assert!(field.flags.is_relative());
        assert_eq!(field.usages[2], Usage::new(0x01, 0x38));
    }

    #[test]
    fn minimal_encoding() {
        let mut b = DescriptorBuilder::new();
        b.usage_page(0x01); // fits one byte -> 05 01
        b.usage_page(0xFF00); // needs two -> 06 00 FF
        let bytes = b.build();
        assert_eq!(bytes, [0x05, 0x01, 0x06, 0x00, 0xFF]);
    }
}
