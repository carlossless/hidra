//! Report descriptor encoder.
//!
//! [`DescriptorBuilder`] emits short items with minimally-sized payloads.
//! The WebHID backend uses it to reconstruct a byte-level report descriptor
//! from the parsed collection data the browser exposes; it is also useful on
//! its own for building descriptors in tests, emulated devices or firmware
//! tooling.

use super::items::{GlobalTag, ItemType, LocalTag, MainTag};
use super::parse::CollectionKind;

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

fn type_bits(item_type: ItemType) -> u8 {
    match item_type {
        ItemType::Main => 0,
        ItemType::Global => 1,
        ItemType::Local => 2,
        ItemType::Reserved => 3,
    }
}

impl DescriptorBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a short item with an unsigned payload, using the smallest
    /// encoding that holds `value`.
    pub fn item_unsigned(&mut self, item_type: ItemType, tag: u8, value: u32) -> &mut Self {
        let data = if value == 0 {
            &[][..]
        } else if value <= 0xFF {
            &value.to_le_bytes()[..1]
        } else if value <= 0xFFFF {
            &value.to_le_bytes()[..2]
        } else {
            &value.to_le_bytes()[..4]
        };
        self.push_item(item_type, tag, data)
    }

    /// Append a short item with a signed payload, using the smallest
    /// encoding that sign-extends back to `value`.
    pub fn item_signed(&mut self, item_type: ItemType, tag: u8, value: i32) -> &mut Self {
        let data = if value == 0 {
            &[][..]
        } else if i8::try_from(value).is_ok() {
            &value.to_le_bytes()[..1]
        } else if i16::try_from(value).is_ok() {
            &value.to_le_bytes()[..2]
        } else {
            &value.to_le_bytes()[..4]
        };
        self.push_item(item_type, tag, data)
    }

    fn push_item(&mut self, item_type: ItemType, tag: u8, data: &[u8]) -> &mut Self {
        debug_assert!(matches!(data.len(), 0 | 1 | 2 | 4));
        debug_assert!(tag <= 0xF);
        let size_code = match data.len() {
            4 => 3,
            n => n as u8,
        };
        self.bytes
            .push((tag << 4) | (type_bits(item_type) << 2) | size_code);
        self.bytes.extend_from_slice(data);
        self
    }

    fn global(tag: GlobalTag) -> u8 {
        match tag {
            GlobalTag::UsagePage => 0x0,
            GlobalTag::LogicalMinimum => 0x1,
            GlobalTag::LogicalMaximum => 0x2,
            GlobalTag::PhysicalMinimum => 0x3,
            GlobalTag::PhysicalMaximum => 0x4,
            GlobalTag::UnitExponent => 0x5,
            GlobalTag::Unit => 0x6,
            GlobalTag::ReportSize => 0x7,
            GlobalTag::ReportId => 0x8,
            GlobalTag::ReportCount => 0x9,
            GlobalTag::Push => 0xA,
            GlobalTag::Pop => 0xB,
        }
    }

    fn local(tag: LocalTag) -> u8 {
        match tag {
            LocalTag::Usage => 0x0,
            LocalTag::UsageMinimum => 0x1,
            LocalTag::UsageMaximum => 0x2,
            LocalTag::DesignatorIndex => 0x3,
            LocalTag::DesignatorMinimum => 0x4,
            LocalTag::DesignatorMaximum => 0x5,
            LocalTag::StringIndex => 0x7,
            LocalTag::StringMinimum => 0x8,
            LocalTag::StringMaximum => 0x9,
            LocalTag::Delimiter => 0xA,
        }
    }

    // --- Global items -----------------------------------------------------

    pub fn usage_page(&mut self, page: u16) -> &mut Self {
        let tag = Self::global(GlobalTag::UsagePage);
        self.item_unsigned(ItemType::Global, tag, page as u32)
    }

    pub fn logical_minimum(&mut self, value: i32) -> &mut Self {
        let tag = Self::global(GlobalTag::LogicalMinimum);
        self.item_signed(ItemType::Global, tag, value)
    }

    pub fn logical_maximum(&mut self, value: i32) -> &mut Self {
        let tag = Self::global(GlobalTag::LogicalMaximum);
        self.item_signed(ItemType::Global, tag, value)
    }

    pub fn physical_minimum(&mut self, value: i32) -> &mut Self {
        let tag = Self::global(GlobalTag::PhysicalMinimum);
        self.item_signed(ItemType::Global, tag, value)
    }

    pub fn physical_maximum(&mut self, value: i32) -> &mut Self {
        let tag = Self::global(GlobalTag::PhysicalMaximum);
        self.item_signed(ItemType::Global, tag, value)
    }

    pub fn unit_exponent(&mut self, value: i32) -> &mut Self {
        let tag = Self::global(GlobalTag::UnitExponent);
        self.item_signed(ItemType::Global, tag, value)
    }

    pub fn unit(&mut self, value: u32) -> &mut Self {
        let tag = Self::global(GlobalTag::Unit);
        self.item_unsigned(ItemType::Global, tag, value)
    }

    pub fn report_size(&mut self, bits: u32) -> &mut Self {
        let tag = Self::global(GlobalTag::ReportSize);
        self.item_unsigned(ItemType::Global, tag, bits)
    }

    pub fn report_id(&mut self, id: u8) -> &mut Self {
        let tag = Self::global(GlobalTag::ReportId);
        self.item_unsigned(ItemType::Global, tag, id as u32)
    }

    pub fn report_count(&mut self, count: u32) -> &mut Self {
        let tag = Self::global(GlobalTag::ReportCount);
        self.item_unsigned(ItemType::Global, tag, count)
    }

    pub fn push(&mut self) -> &mut Self {
        let tag = Self::global(GlobalTag::Push);
        self.push_item(ItemType::Global, tag, &[])
    }

    pub fn pop(&mut self) -> &mut Self {
        let tag = Self::global(GlobalTag::Pop);
        self.push_item(ItemType::Global, tag, &[])
    }

    // --- Local items -------------------------------------------------------

    /// Emit a `Usage` item. Values above `0xFFFF` are emitted as 4-byte
    /// extended usages (page in the high half).
    pub fn usage(&mut self, usage: u32) -> &mut Self {
        let tag = Self::local(LocalTag::Usage);
        self.item_unsigned(ItemType::Local, tag, usage)
    }

    pub fn usage_minimum(&mut self, usage: u32) -> &mut Self {
        let tag = Self::local(LocalTag::UsageMinimum);
        self.item_unsigned(ItemType::Local, tag, usage)
    }

    pub fn usage_maximum(&mut self, usage: u32) -> &mut Self {
        let tag = Self::local(LocalTag::UsageMaximum);
        self.item_unsigned(ItemType::Local, tag, usage)
    }

    pub fn string_index(&mut self, index: u32) -> &mut Self {
        let tag = Self::local(LocalTag::StringIndex);
        self.item_unsigned(ItemType::Local, tag, index)
    }

    // --- Main items ----------------------------------------------------------

    pub fn collection(&mut self, kind: CollectionKind) -> &mut Self {
        self.item_unsigned(ItemType::Main, 0b1010, kind.value() as u32)
    }

    pub fn end_collection(&mut self) -> &mut Self {
        self.push_item(ItemType::Main, 0b1100, &[])
    }

    /// Emit an `Input` item; `flags` is a bit-or of [`super::MainFlags`]
    /// constants.
    pub fn input(&mut self, flags: u32) -> &mut Self {
        self.main(MainTag::Input, flags)
    }

    pub fn output(&mut self, flags: u32) -> &mut Self {
        self.main(MainTag::Output, flags)
    }

    pub fn feature(&mut self, flags: u32) -> &mut Self {
        self.main(MainTag::Feature, flags)
    }

    fn main(&mut self, tag: MainTag, flags: u32) -> &mut Self {
        let tag = match tag {
            MainTag::Input => 0b1000,
            MainTag::Output => 0b1001,
            MainTag::Feature => 0b1011,
            MainTag::Collection => 0b1010,
            MainTag::EndCollection => 0b1100,
        };
        // Input/Output/Feature items conventionally carry at least one data
        // byte even when all flags are zero.
        if flags == 0 {
            self.push_item(ItemType::Main, tag, &[0])
        } else {
            self.item_unsigned(ItemType::Main, tag, flags)
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
