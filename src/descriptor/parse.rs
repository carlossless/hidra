//! Parsed report descriptor model: collections, reports and fields.

use super::items::{GlobalTag, Items, LocalTag, MainTag};
use crate::error::{HidError, HidResult};

/// Direction/class of a HID report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReportKind {
    Input,
    Output,
    Feature,
}

/// Flags carried by Input/Output/Feature main items (HID 1.11, 6.2.2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MainFlags(pub u32);

impl MainFlags {
    pub const CONSTANT: u32 = 1 << 0;
    pub const VARIABLE: u32 = 1 << 1;
    pub const RELATIVE: u32 = 1 << 2;
    pub const WRAP: u32 = 1 << 3;
    pub const NONLINEAR: u32 = 1 << 4;
    pub const NO_PREFERRED: u32 = 1 << 5;
    pub const NULL_STATE: u32 = 1 << 6;
    pub const VOLATILE: u32 = 1 << 7;
    pub const BUFFERED_BYTES: u32 = 1 << 8;

    /// Constant (padding) rather than Data.
    pub fn is_constant(self) -> bool {
        self.0 & Self::CONSTANT != 0
    }
    /// Variable rather than Array.
    pub fn is_variable(self) -> bool {
        self.0 & Self::VARIABLE != 0
    }
    /// Relative rather than Absolute.
    pub fn is_relative(self) -> bool {
        self.0 & Self::RELATIVE != 0
    }
    pub fn is_wrap(self) -> bool {
        self.0 & Self::WRAP != 0
    }
    pub fn is_nonlinear(self) -> bool {
        self.0 & Self::NONLINEAR != 0
    }
    pub fn has_no_preferred_state(self) -> bool {
        self.0 & Self::NO_PREFERRED != 0
    }
    pub fn has_null_state(self) -> bool {
        self.0 & Self::NULL_STATE != 0
    }
    pub fn is_volatile(self) -> bool {
        self.0 & Self::VOLATILE != 0
    }
    pub fn is_buffered_bytes(self) -> bool {
        self.0 & Self::BUFFERED_BYTES != 0
    }
}

/// Collection type (HID 1.11, 6.2.2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionKind {
    Physical,
    Application,
    Logical,
    Report,
    NamedArray,
    UsageSwitch,
    UsageModifier,
    Reserved(u8),
    VendorDefined(u8),
}

impl CollectionKind {
    pub fn from_value(value: u8) -> Self {
        match value {
            0x00 => CollectionKind::Physical,
            0x01 => CollectionKind::Application,
            0x02 => CollectionKind::Logical,
            0x03 => CollectionKind::Report,
            0x04 => CollectionKind::NamedArray,
            0x05 => CollectionKind::UsageSwitch,
            0x06 => CollectionKind::UsageModifier,
            v if v >= 0x80 => CollectionKind::VendorDefined(v),
            v => CollectionKind::Reserved(v),
        }
    }

    pub fn value(self) -> u8 {
        match self {
            CollectionKind::Physical => 0x00,
            CollectionKind::Application => 0x01,
            CollectionKind::Logical => 0x02,
            CollectionKind::Report => 0x03,
            CollectionKind::NamedArray => 0x04,
            CollectionKind::UsageSwitch => 0x05,
            CollectionKind::UsageModifier => 0x06,
            CollectionKind::Reserved(v) | CollectionKind::VendorDefined(v) => v,
        }
    }
}

/// An extended usage: usage page in the high 16 bits, usage ID in the low 16.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Usage(pub u32);

impl Usage {
    pub fn new(page: u16, id: u16) -> Self {
        Usage(((page as u32) << 16) | id as u32)
    }

    pub fn page(self) -> u16 {
        (self.0 >> 16) as u16
    }

    pub fn id(self) -> u16 {
        self.0 as u16
    }
}

/// A collection node in the descriptor's collection tree.
#[derive(Debug, Clone)]
pub struct Collection {
    pub kind: CollectionKind,
    pub usage: Usage,
    pub children: Vec<Collection>,
}

/// One Input/Output/Feature main item: a run of `report_count` elements of
/// `report_size` bits each.
#[derive(Debug, Clone)]
pub struct Field {
    /// Flags from the main item (Constant/Variable/Relative/...).
    pub flags: MainFlags,
    /// Usages assigned to this field by Local `Usage` items.
    pub usages: Vec<Usage>,
    /// Usage range from `Usage Minimum`/`Usage Maximum`, if given.
    pub usage_range: Option<(Usage, Usage)>,
    pub logical_minimum: i32,
    pub logical_maximum: i32,
    pub physical_minimum: Option<i32>,
    pub physical_maximum: Option<i32>,
    pub unit: Option<u32>,
    pub unit_exponent: Option<i32>,
    /// Bits per element.
    pub report_size: u32,
    /// Number of elements.
    pub report_count: u32,
    /// Bit offset of this field within the report payload (the payload does
    /// not include the report ID prefix byte).
    pub bit_offset: u32,
}

impl Field {
    /// Total width of the field in bits (saturating: malformed descriptors
    /// can declare sizes that exceed `u32`).
    pub fn size_bits(&self) -> u32 {
        self.report_size.saturating_mul(self.report_count)
    }
}

/// All fields of one report (one direction + report ID).
#[derive(Debug, Clone)]
pub struct Report {
    pub kind: ReportKind,
    /// `None` when the device does not use report IDs.
    pub report_id: Option<u8>,
    pub fields: Vec<Field>,
}

impl Report {
    /// Payload size in bits (excluding the report ID prefix; saturating).
    pub fn size_bits(&self) -> u32 {
        self.fields
            .iter()
            .fold(0u32, |acc, f| acc.saturating_add(f.size_bits()))
    }

    /// Payload size in bytes, rounded up (excluding the report ID prefix).
    pub fn size_bytes(&self) -> usize {
        self.size_bits().div_ceil(8) as usize
    }
}

/// A parsed HID report descriptor.
#[derive(Debug, Clone, Default)]
pub struct ReportDescriptor {
    /// Top-level collections, in descriptor order.
    pub collections: Vec<Collection>,
    /// All reports declared by the descriptor.
    pub reports: Vec<Report>,
}

impl ReportDescriptor {
    /// Parse a raw report descriptor.
    pub fn parse(bytes: &[u8]) -> HidResult<Self> {
        Parser::default().run(bytes)
    }

    /// Whether the descriptor declares numbered reports.
    pub fn uses_report_ids(&self) -> bool {
        self.reports.iter().any(|r| r.report_id.is_some())
    }

    /// The report for a given direction and ID, if declared.
    pub fn report(&self, kind: ReportKind, report_id: Option<u8>) -> Option<&Report> {
        self.reports
            .iter()
            .find(|r| r.kind == kind && r.report_id == report_id)
    }

    /// Largest payload (in bytes, excluding report ID) among reports of the
    /// given kind. Returns 0 if no such report exists.
    pub fn max_report_size(&self, kind: ReportKind) -> usize {
        self.reports
            .iter()
            .filter(|r| r.kind == kind)
            .map(Report::size_bytes)
            .max()
            .unwrap_or(0)
    }

    /// Largest wire length (in bytes, including the report ID prefix when
    /// the device uses report IDs) among reports of the given kind.
    pub fn max_wire_size(&self, kind: ReportKind) -> usize {
        let extra = usize::from(self.uses_report_ids());
        self.reports
            .iter()
            .filter(|r| r.kind == kind)
            .map(|r| r.size_bytes() + extra)
            .max()
            .unwrap_or(0)
    }

    /// `(usage_page, usage)` of each top-level (application) collection.
    pub fn top_level_usages(&self) -> Vec<(u16, u16)> {
        self.collections
            .iter()
            .map(|c| (c.usage.page(), c.usage.id()))
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
struct GlobalState {
    usage_page: u16,
    logical_minimum: i32,
    logical_maximum: i32,
    physical_minimum: Option<i32>,
    physical_maximum: Option<i32>,
    unit: Option<u32>,
    unit_exponent: Option<i32>,
    report_size: u32,
    report_count: u32,
    report_id: Option<u8>,
}

#[derive(Debug, Clone, Default)]
struct LocalState {
    usages: Vec<Usage>,
    usage_minimum: Option<u32>,
    usage_maximum: Option<u32>,
}

#[derive(Default)]
struct Parser {
    globals: GlobalState,
    global_stack: Vec<GlobalState>,
    locals: LocalState,
    collection_stack: Vec<Collection>,
    collections: Vec<Collection>,
    reports: Vec<Report>,
    /// Running bit offset per (kind, report id).
    offsets: std::collections::HashMap<(ReportKind, Option<u8>), u32>,
}

impl Parser {
    fn run(mut self, bytes: &[u8]) -> HidResult<ReportDescriptor> {
        for item in Items::new(bytes) {
            let item = item?;
            if item.long {
                // Long items are reserved; skip like every real-world parser.
                continue;
            }
            if let Some(tag) = item.global_tag() {
                self.global_item(tag, item.unsigned(), item.signed())?;
            } else if let Some(tag) = item.local_tag() {
                self.local_item(tag, item.unsigned());
            } else if let Some(tag) = item.main_tag() {
                self.main_item(tag, item.unsigned())?;
                self.locals = LocalState::default();
            }
            // Unknown/reserved tags are skipped.
        }
        if !self.collection_stack.is_empty() {
            return Err(HidError::Parse {
                message: "unterminated collection".into(),
            });
        }
        Ok(ReportDescriptor {
            collections: self.collections,
            reports: self.reports,
        })
    }

    fn global_item(&mut self, tag: GlobalTag, unsigned: u32, signed: i32) -> HidResult<()> {
        match tag {
            GlobalTag::UsagePage => self.globals.usage_page = unsigned as u16,
            GlobalTag::LogicalMinimum => self.globals.logical_minimum = signed,
            GlobalTag::LogicalMaximum => self.globals.logical_maximum = signed,
            GlobalTag::PhysicalMinimum => self.globals.physical_minimum = Some(signed),
            GlobalTag::PhysicalMaximum => self.globals.physical_maximum = Some(signed),
            GlobalTag::UnitExponent => self.globals.unit_exponent = Some(signed),
            GlobalTag::Unit => self.globals.unit = Some(unsigned),
            GlobalTag::ReportSize => self.globals.report_size = unsigned,
            GlobalTag::ReportId => self.globals.report_id = Some(unsigned as u8),
            GlobalTag::ReportCount => self.globals.report_count = unsigned,
            GlobalTag::Push => self.global_stack.push(self.globals.clone()),
            GlobalTag::Pop => {
                self.globals = self.global_stack.pop().ok_or_else(|| HidError::Parse {
                    message: "Pop without matching Push".into(),
                })?;
            }
        }
        Ok(())
    }

    fn local_item(&mut self, tag: LocalTag, unsigned: u32) {
        match tag {
            LocalTag::Usage => self.locals.usages.push(self.extended_usage(unsigned)),
            LocalTag::UsageMinimum => self.locals.usage_minimum = Some(unsigned),
            LocalTag::UsageMaximum => self.locals.usage_maximum = Some(unsigned),
            // Designator/string associations and delimiters are not modeled.
            _ => {}
        }
    }

    /// Usage items with 4-byte data carry their own usage page in the high
    /// half; shorter ones inherit the current global usage page.
    fn extended_usage(&self, raw: u32) -> Usage {
        if raw > 0xFFFF {
            Usage(raw)
        } else {
            Usage::new(self.globals.usage_page, raw as u16)
        }
    }

    fn main_item(&mut self, tag: MainTag, data: u32) -> HidResult<()> {
        match tag {
            MainTag::Collection => {
                let usage = self
                    .locals
                    .usages
                    .first()
                    .copied()
                    .unwrap_or(Usage::new(self.globals.usage_page, 0));
                self.collection_stack.push(Collection {
                    kind: CollectionKind::from_value(data as u8),
                    usage,
                    children: Vec::new(),
                });
            }
            MainTag::EndCollection => {
                let done = self.collection_stack.pop().ok_or_else(|| HidError::Parse {
                    message: "End Collection without matching Collection".into(),
                })?;
                match self.collection_stack.last_mut() {
                    Some(parent) => parent.children.push(done),
                    None => self.collections.push(done),
                }
            }
            MainTag::Input | MainTag::Output | MainTag::Feature => {
                let kind = match tag {
                    MainTag::Input => ReportKind::Input,
                    MainTag::Output => ReportKind::Output,
                    _ => ReportKind::Feature,
                };
                self.add_field(kind, MainFlags(data));
            }
        }
        Ok(())
    }

    fn add_field(&mut self, kind: ReportKind, flags: MainFlags) {
        let usage_range = match (self.locals.usage_minimum, self.locals.usage_maximum) {
            (Some(min), Some(max)) => Some((self.extended_usage(min), self.extended_usage(max))),
            _ => None,
        };
        let g = &self.globals;
        let offset = self.offsets.entry((kind, g.report_id)).or_insert(0);
        let field = Field {
            flags,
            usages: self.locals.usages.clone(),
            usage_range,
            logical_minimum: g.logical_minimum,
            logical_maximum: g.logical_maximum,
            physical_minimum: g.physical_minimum,
            physical_maximum: g.physical_maximum,
            unit: g.unit,
            unit_exponent: g.unit_exponent,
            report_size: g.report_size,
            report_count: g.report_count,
            bit_offset: *offset,
        };
        // Vendor descriptors can declare absurd sizes (4-byte Report Size /
        // Report Count items); saturate instead of overflowing.
        *offset = offset.saturating_add(field.size_bits());

        match self
            .reports
            .iter_mut()
            .find(|r| r.kind == kind && r.report_id == g.report_id)
        {
            Some(report) => report.fields.push(field),
            None => self.reports.push(Report {
                kind,
                report_id: g.report_id,
                fields: vec![field],
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Standard boot-protocol keyboard descriptor (HID 1.11, appendix B.1).
    pub(crate) const KEYBOARD: &[u8] = &[
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x06, // Usage (Keyboard)
        0xA1, 0x01, // Collection (Application)
        0x05, 0x07, //   Usage Page (Key Codes)
        0x19, 0xE0, //   Usage Minimum (224)
        0x29, 0xE7, //   Usage Maximum (231)
        0x15, 0x00, //   Logical Minimum (0)
        0x25, 0x01, //   Logical Maximum (1)
        0x75, 0x01, //   Report Size (1)
        0x95, 0x08, //   Report Count (8)
        0x81, 0x02, //   Input (Data, Variable, Absolute) ; modifiers
        0x95, 0x01, //   Report Count (1)
        0x75, 0x08, //   Report Size (8)
        0x81, 0x01, //   Input (Constant) ; reserved byte
        0x95, 0x05, //   Report Count (5)
        0x75, 0x01, //   Report Size (1)
        0x05, 0x08, //   Usage Page (LEDs)
        0x19, 0x01, //   Usage Minimum (1)
        0x29, 0x05, //   Usage Maximum (5)
        0x91, 0x02, //   Output (Data, Variable, Absolute) ; LEDs
        0x95, 0x01, //   Report Count (1)
        0x75, 0x03, //   Report Size (3)
        0x91, 0x01, //   Output (Constant) ; padding
        0x95, 0x06, //   Report Count (6)
        0x75, 0x08, //   Report Size (8)
        0x15, 0x00, //   Logical Minimum (0)
        0x25, 0x65, //   Logical Maximum (101)
        0x05, 0x07, //   Usage Page (Key Codes)
        0x19, 0x00, //   Usage Minimum (0)
        0x29, 0x65, //   Usage Maximum (101)
        0x81, 0x00, //   Input (Data, Array) ; key array
        0xC0, // End Collection
    ];

    #[test]
    fn parses_boot_keyboard() {
        let desc = ReportDescriptor::parse(KEYBOARD).unwrap();
        assert_eq!(desc.collections.len(), 1);
        let top = &desc.collections[0];
        assert_eq!(top.kind, CollectionKind::Application);
        assert_eq!(top.usage, Usage::new(0x01, 0x06));

        assert!(!desc.uses_report_ids());
        let input = desc.report(ReportKind::Input, None).unwrap();
        assert_eq!(input.size_bytes(), 8);
        let output = desc.report(ReportKind::Output, None).unwrap();
        assert_eq!(output.size_bytes(), 1);

        // Modifier field: 8 one-bit variables at offset 0.
        let modifiers = &input.fields[0];
        assert!(modifiers.flags.is_variable());
        assert_eq!(modifiers.report_size, 1);
        assert_eq!(modifiers.report_count, 8);
        assert_eq!(modifiers.bit_offset, 0);
        assert_eq!(
            modifiers.usage_range,
            Some((Usage::new(0x07, 0xE0), Usage::new(0x07, 0xE7)))
        );

        // Key array: starts after modifiers (8 bits) + reserved byte (8 bits).
        let keys = &input.fields[2];
        assert!(!keys.flags.is_variable());
        assert_eq!(keys.bit_offset, 16);
        assert_eq!(desc.max_report_size(ReportKind::Input), 8);
        assert_eq!(desc.max_wire_size(ReportKind::Input), 8);
    }

    #[test]
    fn report_ids_add_wire_byte() {
        // Two numbered feature reports of different sizes.
        let desc_bytes = [
            0x06, 0x00, 0xFF, // Usage Page (Vendor 0xFF00)
            0x09, 0x01, // Usage (1)
            0xA1, 0x01, // Collection (Application)
            0x85, 0x01, //   Report ID (1)
            0x75, 0x08, //   Report Size (8)
            0x95, 0x10, //   Report Count (16)
            0x09, 0x02, //   Usage (2)
            0xB1, 0x02, //   Feature (Data, Variable, Absolute)
            0x85, 0x02, //   Report ID (2)
            0x95, 0x20, //   Report Count (32)
            0x09, 0x03, //   Usage (3)
            0xB1, 0x02, //   Feature (Data, Variable, Absolute)
            0xC0, // End Collection
        ];
        let desc = ReportDescriptor::parse(&desc_bytes).unwrap();
        assert!(desc.uses_report_ids());
        assert_eq!(
            desc.report(ReportKind::Feature, Some(1))
                .unwrap()
                .size_bytes(),
            16
        );
        assert_eq!(
            desc.report(ReportKind::Feature, Some(2))
                .unwrap()
                .size_bytes(),
            32
        );
        assert_eq!(desc.max_report_size(ReportKind::Feature), 32);
        assert_eq!(desc.max_wire_size(ReportKind::Feature), 33);
        assert_eq!(desc.top_level_usages(), vec![(0xFF00, 0x01)]);
    }

    #[test]
    fn push_pop_round_trips() {
        let desc_bytes = [
            0x05, 0x01, // Usage Page (Generic Desktop)
            0x09, 0x00, // Usage (0)
            0xA1, 0x01, // Collection (Application)
            0x75, 0x08, //   Report Size (8)
            0xA4, //   Push
            0x75, 0x10, //   Report Size (16)
            0xB4, //   Pop
            0x95, 0x01, //   Report Count (1)
            0x09, 0x01, //   Usage (1)
            0x81, 0x02, //   Input
            0xC0, // End Collection
        ];
        let desc = ReportDescriptor::parse(&desc_bytes).unwrap();
        let input = desc.report(ReportKind::Input, None).unwrap();
        assert_eq!(input.fields[0].report_size, 8);
    }

    #[test]
    fn absurd_report_sizes_saturate_instead_of_overflowing() {
        // Seen in the wild on vendor devices: 4-byte Report Size / Report
        // Count items whose product exceeds u32. Parsing must not panic
        // (debug builds previously overflowed in the offset accumulator).
        let desc_bytes = [
            0x06, 0x00, 0xFF, // Usage Page (Vendor 0xFF00)
            0x09, 0x01, // Usage (1)
            0xA1, 0x01, // Collection (Application)
            0x77, 0xFF, 0xFF, 0xFF, 0xFF, //   Report Size (0xFFFFFFFF)
            0x97, 0xFF, 0xFF, 0xFF, 0xFF, //   Report Count (0xFFFFFFFF)
            0x09, 0x02, //   Usage (2)
            0x81, 0x02, //   Input
            0x09, 0x03, //   Usage (3)
            0x81, 0x02, //   Input (second field: offset would overflow)
            0xC0, // End Collection
        ];
        let desc = ReportDescriptor::parse(&desc_bytes).unwrap();
        let report = desc.report(ReportKind::Input, None).unwrap();
        assert_eq!(report.size_bits(), u32::MAX);
        assert_eq!(report.fields[1].bit_offset, u32::MAX);
    }

    #[test]
    fn unbalanced_collections_error() {
        let desc_bytes = [0x05, 0x01, 0x09, 0x06, 0xA1, 0x01];
        assert!(ReportDescriptor::parse(&desc_bytes).is_err());
    }
}
