//! WebHID-style collection primitives, also usable standalone.
//!
//! [`CollectionInfo`] / [`ReportInfo`] / [`ReportItemInfo`] mirror the data
//! the [WebHID](https://wicg.github.io/webhid/) API exposes for each device
//! (`HIDCollectionInfo`, `HIDReportInfo`, `HIDReportItem`): the collection
//! tree with every report and the per-item flags, ranges and units, i.e. a
//! browser-parsed view of the report descriptor.
//!
//! [`reconstruct_descriptor`] turns that view back into raw descriptor bytes
//! using [`crate::descriptor::DescriptorBuilder`]. The WebHID backend uses it
//! to implement `report_descriptor()` (browsers never expose the original
//! bytes), but the types are plain data and compiled on every target, so the
//! reconstruction can also be used and tested off-browser.

use crate::descriptor::{CollectionKind, DescriptorBuilder, MainFlags, ReportKind};

/// Unit system of a report item, mirroring WebHID's `HIDUnitSystem`
/// (the low nibble of a HID `Unit` item).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UnitSystem {
    /// No unit (`Unit` nibble 0).
    #[default]
    None,
    /// SI linear (centimeter, gram, second, kelvin, ampere, candela).
    SiLinear,
    /// SI rotation (radian, gram, second, kelvin, ampere, candela).
    SiRotation,
    /// English linear (inch, slug, second, fahrenheit, ampere, candela).
    EnglishLinear,
    /// English rotation (degree, slug, second, fahrenheit, ampere, candela).
    EnglishRotation,
    /// Vendor-defined system (nibble 0xF).
    VendorDefined,
    /// A reserved system nibble (5..=0xE).
    Reserved,
}

impl UnitSystem {
    /// The system nibble used in a HID `Unit` item.
    pub fn nibble(self) -> u32 {
        match self {
            UnitSystem::None | UnitSystem::Reserved => 0x0,
            UnitSystem::SiLinear => 0x1,
            UnitSystem::SiRotation => 0x2,
            UnitSystem::EnglishLinear => 0x3,
            UnitSystem::EnglishRotation => 0x4,
            UnitSystem::VendorDefined => 0xF,
        }
    }
}

/// One collection node, mirroring WebHID's `HIDCollectionInfo`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CollectionInfo {
    /// Usage page of the collection's usage.
    pub usage_page: u16,
    /// Usage ID of the collection's usage.
    pub usage: u16,
    /// Raw collection type byte (0 = Physical, 1 = Application, ...); see
    /// [`CollectionKind::from_value`].
    pub collection_type: u8,
    /// Nested collections, in descriptor order.
    pub children: Vec<CollectionInfo>,
    /// Input reports declared at this level.
    pub input_reports: Vec<ReportInfo>,
    /// Output reports declared at this level.
    pub output_reports: Vec<ReportInfo>,
    /// Feature reports declared at this level.
    pub feature_reports: Vec<ReportInfo>,
}

/// One report of a single direction, mirroring WebHID's `HIDReportInfo`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReportInfo {
    /// Report ID; `0` when the device does not use numbered reports.
    pub report_id: u8,
    /// The fields of the report, in descriptor order.
    pub items: Vec<ReportItemInfo>,
}

/// One Input/Output/Feature main item, mirroring WebHID's `HIDReportItem`.
///
/// Usages are 32-bit extended usages: usage page in the high 16 bits, usage
/// ID in the low 16, matching [`crate::descriptor::Usage`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReportItemInfo {
    /// Usages assigned to the item (when [`is_range`](Self::is_range) is
    /// `false`).
    pub usages: Vec<u32>,
    /// First usage of the range (when [`is_range`](Self::is_range) is `true`).
    pub usage_minimum: u32,
    /// Last usage of the range (when [`is_range`](Self::is_range) is `true`).
    pub usage_maximum: u32,
    /// Bits per element (`Report Size`).
    pub report_size: u16,
    /// Number of elements (`Report Count`).
    pub report_count: u16,
    pub logical_minimum: i32,
    pub logical_maximum: i32,
    pub physical_minimum: i32,
    pub physical_maximum: i32,
    /// `Unit Exponent` (power of ten applied to the unit).
    pub unit_exponent: i8,
    /// Unit system nibble of the `Unit` item.
    pub unit_system: UnitSystem,
    /// Length (cm/radian/inch/degree) exponent nibble of the `Unit` item.
    pub unit_factor_length_exponent: i8,
    /// Mass (gram/slug) exponent nibble of the `Unit` item.
    pub unit_factor_mass_exponent: i8,
    /// Time (seconds) exponent nibble of the `Unit` item.
    pub unit_factor_time_exponent: i8,
    /// Temperature (kelvin/fahrenheit) exponent nibble of the `Unit` item.
    pub unit_factor_temperature_exponent: i8,
    /// Current (ampere) exponent nibble of the `Unit` item.
    pub unit_factor_current_exponent: i8,
    /// Luminous intensity (candela) exponent nibble of the `Unit` item.
    pub unit_factor_luminous_intensity_exponent: i8,
    /// Absolute rather than Relative.
    pub is_absolute: bool,
    /// Array rather than Variable.
    pub is_array: bool,
    /// Buffered Bytes rather than Bit Field.
    pub is_buffered_bytes: bool,
    /// Constant (padding) rather than Data.
    pub is_constant: bool,
    /// Linear rather than Non Linear.
    pub is_linear: bool,
    /// The usages are given as a `Usage Minimum`/`Usage Maximum` range
    /// rather than a list.
    pub is_range: bool,
    /// Volatile rather than Non Volatile.
    pub is_volatile: bool,
    /// Has a Null State.
    pub has_null: bool,
    /// Preferred State rather than No Preferred.
    pub has_preferred_state: bool,
    /// Wrap rather than No Wrap.
    pub wrap: bool,
}

impl Default for ReportItemInfo {
    /// Defaults match an all-zero main item: Data, Variable, Absolute,
    /// Linear, Preferred State, no usages, no unit.
    fn default() -> Self {
        ReportItemInfo {
            usages: Vec::new(),
            usage_minimum: 0,
            usage_maximum: 0,
            report_size: 0,
            report_count: 0,
            logical_minimum: 0,
            logical_maximum: 0,
            physical_minimum: 0,
            physical_maximum: 0,
            unit_exponent: 0,
            unit_system: UnitSystem::None,
            unit_factor_length_exponent: 0,
            unit_factor_mass_exponent: 0,
            unit_factor_time_exponent: 0,
            unit_factor_temperature_exponent: 0,
            unit_factor_current_exponent: 0,
            unit_factor_luminous_intensity_exponent: 0,
            is_absolute: true,
            is_array: false,
            is_buffered_bytes: false,
            is_constant: false,
            is_linear: true,
            is_range: false,
            is_volatile: false,
            has_null: false,
            has_preferred_state: true,
            wrap: false,
        }
    }
}

impl ReportItemInfo {
    /// The 32-bit HID `Unit` value encoding the system and factor exponent
    /// nibbles (HID 1.11, 6.2.2.7).
    pub fn unit_value(&self) -> u32 {
        fn nibble(exponent: i8) -> u32 {
            (exponent as u32) & 0xF
        }
        self.unit_system.nibble()
            | nibble(self.unit_factor_length_exponent) << 4
            | nibble(self.unit_factor_mass_exponent) << 8
            | nibble(self.unit_factor_time_exponent) << 12
            | nibble(self.unit_factor_temperature_exponent) << 16
            | nibble(self.unit_factor_current_exponent) << 20
            | nibble(self.unit_factor_luminous_intensity_exponent) << 24
    }

    /// [`MainFlags`] bits for the Input/Output/Feature item this describes.
    pub fn main_flags(&self) -> u32 {
        let mut flags = 0;
        if self.is_constant {
            flags |= MainFlags::CONSTANT;
        }
        if !self.is_array {
            flags |= MainFlags::VARIABLE;
        }
        if !self.is_absolute {
            flags |= MainFlags::RELATIVE;
        }
        if self.wrap {
            flags |= MainFlags::WRAP;
        }
        if !self.is_linear {
            flags |= MainFlags::NONLINEAR;
        }
        if !self.has_preferred_state {
            flags |= MainFlags::NO_PREFERRED;
        }
        if self.has_null {
            flags |= MainFlags::NULL_STATE;
        }
        if self.is_volatile {
            flags |= MainFlags::VOLATILE;
        }
        if self.is_buffered_bytes {
            flags |= MainFlags::BUFFERED_BYTES;
        }
        flags
    }
}

/// Whether any report anywhere in the collection tree is numbered, matching
/// [`crate::descriptor::ReportDescriptor::uses_report_ids`].
pub fn uses_report_ids(collections: &[CollectionInfo]) -> bool {
    collections.iter().any(|c| {
        c.input_reports
            .iter()
            .chain(&c.output_reports)
            .chain(&c.feature_reports)
            .any(|r| r.report_id != 0)
            || uses_report_ids(&c.children)
    })
}

/// Reconstruct a raw HID report descriptor from WebHID collection data.
///
/// Browsers never expose the descriptor bytes a device reported, only the
/// parsed [`CollectionInfo`] tree; this re-encodes that tree with
/// [`DescriptorBuilder`]. The result parses back with
/// [`crate::descriptor::ReportDescriptor`] to the same report IDs, sizes,
/// flags and usages, but is not byte-identical to the original descriptor
/// (item order and encodings are normalized, and anything WebHID does not
/// model, designators, strings, delimiters, push/pop, is lost).
pub fn reconstruct_descriptor(collections: &[CollectionInfo]) -> Vec<u8> {
    let mut builder = DescriptorBuilder::new();
    for collection in collections {
        emit_collection(&mut builder, collection);
    }
    builder.build()
}

fn emit_collection(builder: &mut DescriptorBuilder, collection: &CollectionInfo) {
    builder
        .usage_page(collection.usage_page)
        .usage(collection.usage as u32)
        .collection(CollectionKind::from_value(collection.collection_type));
    for report in &collection.input_reports {
        emit_report(builder, ReportKind::Input, report);
    }
    for report in &collection.output_reports {
        emit_report(builder, ReportKind::Output, report);
    }
    for report in &collection.feature_reports {
        emit_report(builder, ReportKind::Feature, report);
    }
    for child in &collection.children {
        emit_collection(builder, child);
    }
    builder.end_collection();
}

fn emit_report(builder: &mut DescriptorBuilder, kind: ReportKind, report: &ReportInfo) {
    if report.report_id != 0 {
        builder.report_id(report.report_id);
    }
    for item in &report.items {
        // Globals are emitted for every item (even when zero) so that no
        // state accidentally carries over from the previous item.
        builder
            .logical_minimum(item.logical_minimum)
            .logical_maximum(item.logical_maximum)
            .physical_minimum(item.physical_minimum)
            .physical_maximum(item.physical_maximum)
            .unit(item.unit_value())
            .unit_exponent(item.unit_exponent as i32)
            .report_size(item.report_size as u32)
            .report_count(item.report_count as u32);
        if item.is_range {
            builder
                .usage_minimum(item.usage_minimum)
                .usage_maximum(item.usage_maximum);
        } else {
            for &usage in &item.usages {
                builder.usage(usage);
            }
        }
        let flags = item.main_flags();
        match kind {
            ReportKind::Input => builder.input(flags),
            ReportKind::Output => builder.output(flags),
            ReportKind::Feature => builder.feature(flags),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{ReportDescriptor, ReportKind, Usage};

    /// Boot keyboard as WebHID would report it: one application collection,
    /// unnumbered input and output reports.
    fn keyboard() -> Vec<CollectionInfo> {
        vec![CollectionInfo {
            usage_page: 0x01,
            usage: 0x06,
            collection_type: CollectionKind::Application.value(),
            input_reports: vec![ReportInfo {
                report_id: 0,
                items: vec![
                    // Modifier bits.
                    ReportItemInfo {
                        is_range: true,
                        usage_minimum: 0x0007_00E0,
                        usage_maximum: 0x0007_00E7,
                        logical_minimum: 0,
                        logical_maximum: 1,
                        report_size: 1,
                        report_count: 8,
                        ..Default::default()
                    },
                    // Reserved byte.
                    ReportItemInfo {
                        is_constant: true,
                        report_size: 8,
                        report_count: 1,
                        ..Default::default()
                    },
                    // Key array.
                    ReportItemInfo {
                        is_array: true,
                        is_range: true,
                        usage_minimum: 0x0007_0000,
                        usage_maximum: 0x0007_0065,
                        logical_minimum: 0,
                        logical_maximum: 101,
                        report_size: 8,
                        report_count: 6,
                        ..Default::default()
                    },
                ],
            }],
            output_reports: vec![ReportInfo {
                report_id: 0,
                items: vec![
                    // LEDs.
                    ReportItemInfo {
                        is_range: true,
                        usage_minimum: 0x0008_0001,
                        usage_maximum: 0x0008_0005,
                        logical_minimum: 0,
                        logical_maximum: 1,
                        report_size: 1,
                        report_count: 5,
                        ..Default::default()
                    },
                    // Padding.
                    ReportItemInfo {
                        is_constant: true,
                        report_size: 3,
                        report_count: 1,
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        }]
    }

    #[test]
    fn keyboard_round_trips() {
        let collections = keyboard();
        assert!(!uses_report_ids(&collections));

        let bytes = reconstruct_descriptor(&collections);
        let parsed = ReportDescriptor::parse(&bytes).unwrap();
        assert!(!parsed.uses_report_ids());
        assert_eq!(parsed.collections.len(), 1);
        assert_eq!(parsed.collections[0].kind, CollectionKind::Application);
        assert_eq!(parsed.top_level_usages(), vec![(0x01, 0x06)]);

        let input = parsed.report(ReportKind::Input, None).unwrap();
        assert_eq!(input.size_bytes(), 8);
        let modifiers = &input.fields[0];
        assert!(modifiers.flags.is_variable());
        assert!(!modifiers.flags.is_relative());
        assert_eq!(
            modifiers.usage_range,
            Some((Usage::new(0x07, 0xE0), Usage::new(0x07, 0xE7)))
        );
        assert!(input.fields[1].flags.is_constant());
        let keys = &input.fields[2];
        assert!(!keys.flags.is_variable());
        assert_eq!(keys.bit_offset, 16);
        assert_eq!(keys.logical_maximum, 101);

        let output = parsed.report(ReportKind::Output, None).unwrap();
        assert_eq!(output.size_bytes(), 1);
    }

    #[test]
    fn numbered_feature_reports_round_trip() {
        let collections = vec![CollectionInfo {
            usage_page: 0xFF00,
            usage: 0x01,
            collection_type: CollectionKind::Application.value(),
            feature_reports: vec![
                ReportInfo {
                    report_id: 1,
                    items: vec![ReportItemInfo {
                        usages: vec![0xFF00_0002],
                        logical_minimum: 0,
                        logical_maximum: 255,
                        report_size: 8,
                        report_count: 16,
                        ..Default::default()
                    }],
                },
                ReportInfo {
                    report_id: 2,
                    items: vec![ReportItemInfo {
                        usages: vec![0xFF00_0003],
                        logical_minimum: 0,
                        logical_maximum: 255,
                        report_size: 8,
                        report_count: 32,
                        ..Default::default()
                    }],
                },
            ],
            ..Default::default()
        }];
        assert!(uses_report_ids(&collections));

        let bytes = reconstruct_descriptor(&collections);
        let parsed = ReportDescriptor::parse(&bytes).unwrap();
        assert!(parsed.uses_report_ids());
        let r1 = parsed.report(ReportKind::Feature, Some(1)).unwrap();
        assert_eq!(r1.size_bytes(), 16);
        assert_eq!(r1.fields[0].usages, vec![Usage::new(0xFF00, 0x02)]);
        let r2 = parsed.report(ReportKind::Feature, Some(2)).unwrap();
        assert_eq!(r2.size_bytes(), 32);
        assert_eq!(parsed.max_wire_size(ReportKind::Feature), 33);
        assert_eq!(parsed.top_level_usages(), vec![(0xFF00, 0x01)]);
    }

    #[test]
    fn nested_collections_units_and_flags_round_trip() {
        // Mouse-like: application collection wrapping a physical collection
        // with relative X/Y axes carrying a unit (SI linear, cm * 10^-2).
        let collections = vec![CollectionInfo {
            usage_page: 0x01,
            usage: 0x02,
            collection_type: CollectionKind::Application.value(),
            children: vec![CollectionInfo {
                usage_page: 0x01,
                usage: 0x01,
                collection_type: CollectionKind::Physical.value(),
                input_reports: vec![ReportInfo {
                    report_id: 3,
                    items: vec![ReportItemInfo {
                        usages: vec![0x0001_0030, 0x0001_0031],
                        logical_minimum: -127,
                        logical_maximum: 127,
                        physical_minimum: -127,
                        physical_maximum: 127,
                        report_size: 8,
                        report_count: 2,
                        is_absolute: false,
                        is_linear: false,
                        has_preferred_state: false,
                        has_null: true,
                        wrap: true,
                        unit_system: UnitSystem::SiLinear,
                        unit_factor_length_exponent: 1,
                        unit_exponent: -2,
                        ..Default::default()
                    }],
                }],
                ..Default::default()
            }],
            ..Default::default()
        }];

        let bytes = reconstruct_descriptor(&collections);
        let parsed = ReportDescriptor::parse(&bytes).unwrap();
        let top = &parsed.collections[0];
        assert_eq!(top.usage, Usage::new(0x01, 0x02));
        assert_eq!(top.children.len(), 1);
        assert_eq!(top.children[0].kind, CollectionKind::Physical);

        let input = parsed.report(ReportKind::Input, Some(3)).unwrap();
        let axes = &input.fields[0];
        assert!(axes.flags.is_relative());
        assert!(axes.flags.is_wrap());
        assert!(axes.flags.is_nonlinear());
        assert!(axes.flags.has_no_preferred_state());
        assert!(axes.flags.has_null_state());
        assert_eq!(axes.logical_minimum, -127);
        assert_eq!(axes.physical_maximum, Some(127));
        assert_eq!(axes.unit, Some(0x11)); // SI linear, length^1
        assert_eq!(axes.unit_exponent, Some(-2));
        assert_eq!(
            axes.usages,
            vec![Usage::new(0x01, 0x30), Usage::new(0x01, 0x31)]
        );
    }

    #[test]
    fn unit_value_encodes_negative_exponents() {
        let item = ReportItemInfo {
            unit_system: UnitSystem::SiLinear,
            unit_factor_length_exponent: 1,
            unit_factor_time_exponent: -2, // e.g. cm/s^2
            ..Default::default()
        };
        assert_eq!(item.unit_value(), 0x0000_E011);
    }
}
