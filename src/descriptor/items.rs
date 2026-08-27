//! Raw HID report descriptor item lexer.
//!
//! These are the lowest-level primitives: an iterator over the short/long
//! items of a report descriptor, plus typed tags. See Device Class Definition
//! for HID 1.11, section 6.2.2.

use crate::error::{HidError, HidResult};

/// Declares a short-item tag enum together with the `bTag` values the HID
/// spec assigns it.
///
/// Encoding and decoding both come from this one list, so a tag cannot be
/// read as one item and written back as another. `build.rs` used to carry a
/// hand-written inverse of the tables below.
macro_rules! item_tags {
    (
        $(#[$enum_meta:meta])*
        pub enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident = $code:literal, )*
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name {
            $( $(#[$variant_meta])* $variant, )*
        }

        impl $name {
            /// The `bTag` nibble this item uses in a short-item prefix.
            pub const fn code(self) -> u8 {
                match self {
                    $( $name::$variant => $code, )*
                }
            }

            /// The item this `bTag` nibble denotes, or `None` when the HID
            /// spec assigns it no meaning.
            pub const fn from_code(code: u8) -> Option<Self> {
                match code {
                    $( $code => Some($name::$variant), )*
                    _ => None,
                }
            }
        }
    };
}

/// `bType` of a short item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemType {
    /// Main item (Input/Output/Feature/Collection/`EndCollection`).
    Main,
    /// Global item (state that persists across items, e.g. usage page).
    Global,
    /// Local item (state that applies only to the next Main item).
    Local,
    /// Reserved type, also used for long items.
    Reserved,
}

impl ItemType {
    /// The 2-bit `bType` field of a short-item prefix.
    pub const fn code(self) -> u8 {
        match self {
            ItemType::Main => 0,
            ItemType::Global => 1,
            ItemType::Local => 2,
            ItemType::Reserved => 3,
        }
    }

    /// The item type a 2-bit `bType` field denotes. Total: every value of the
    /// field is defined, with 3 reserved.
    pub const fn from_code(code: u8) -> Self {
        match code & 0x3 {
            0 => ItemType::Main,
            1 => ItemType::Global,
            2 => ItemType::Local,
            _ => ItemType::Reserved,
        }
    }
}

item_tags! {
    /// Tags of Main items (HID 1.11, 6.2.2.4).
    pub enum MainTag {
        /// Input item: a data field reported by the device.
        Input = 0b1000,
        /// Output item: a data field sent to the device.
        Output = 0b1001,
        /// Feature item: a data field for configuration, both directions.
        Feature = 0b1011,
        /// Collection: opens a grouping of items; data is the collection type.
        Collection = 0b1010,
        /// End Collection: closes the most recently opened collection.
        EndCollection = 0b1100,
    }
}

item_tags! {
    /// Tags of Global items (HID 1.11, 6.2.2.7).
    pub enum GlobalTag {
        /// Usage Page: the usage page applied to subsequent usages.
        UsagePage = 0x0,
        /// Logical Minimum: minimum value a field can report.
        LogicalMinimum = 0x1,
        /// Logical Maximum: maximum value a field can report.
        LogicalMaximum = 0x2,
        /// Physical Minimum: logical minimum mapped to physical units.
        PhysicalMinimum = 0x3,
        /// Physical Maximum: logical maximum mapped to physical units.
        PhysicalMaximum = 0x4,
        /// Unit Exponent: base-10 exponent applied to physical units.
        UnitExponent = 0x5,
        /// Unit: encoded physical unit of a field.
        Unit = 0x6,
        /// Report Size: size of each field, in bits.
        ReportSize = 0x7,
        /// Report ID: identifier prefixed to subsequent reports.
        ReportId = 0x8,
        /// Report Count: number of fields in the item.
        ReportCount = 0x9,
        /// Push: save the current global item state onto the stack.
        Push = 0xA,
        /// Pop: restore the global item state from the stack.
        Pop = 0xB,
    }
}

item_tags! {
    /// Tags of Local items (HID 1.11, 6.2.2.8).
    pub enum LocalTag {
        /// Usage: a usage assigned to the next field(s).
        Usage = 0x0,
        /// Usage Minimum: start of a range of usages.
        UsageMinimum = 0x1,
        /// Usage Maximum: end of a range of usages.
        UsageMaximum = 0x2,
        /// Designator Index: physical designator from the physical descriptor.
        DesignatorIndex = 0x3,
        /// Designator Minimum: start of a range of designators.
        DesignatorMinimum = 0x4,
        /// Designator Maximum: end of a range of designators.
        DesignatorMaximum = 0x5,
        /// String Index: index of a string descriptor for the field.
        StringIndex = 0x7,
        /// String Minimum: start of a range of string indices.
        StringMinimum = 0x8,
        /// String Maximum: end of a range of string indices.
        StringMaximum = 0x9,
        /// Delimiter: opens or closes a set of alternative usages.
        Delimiter = 0xA,
    }
}

/// A single item of a report descriptor, borrowed from the input buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawItem<'a> {
    /// `bType` for short items; long items (prefix `0xFE`) report
    /// [`ItemType::Reserved`].
    pub item_type: ItemType,
    /// 4-bit tag for short items, `bLongItemTag` for long items.
    pub tag: u8,
    /// Item payload (0, 1, 2 or 4 bytes for short items).
    pub data: &'a [u8],
    /// Whether this is a long item.
    pub long: bool,
}

impl<'a> RawItem<'a> {
    /// Payload interpreted as a little-endian unsigned integer.
    pub fn unsigned(&self) -> u32 {
        let mut v: u32 = 0;
        for (i, b) in self.data.iter().take(4).enumerate() {
            v |= (*b as u32) << (8 * i);
        }
        v
    }

    /// Payload interpreted as a little-endian signed (sign-extended) integer.
    pub fn signed(&self) -> i32 {
        match self.data.len() {
            0 => 0,
            1 => self.data[0] as i8 as i32,
            2 => i16::from_le_bytes([self.data[0], self.data[1]]) as i32,
            _ => self.unsigned() as i32,
        }
    }

    /// Typed Main tag, if this is a recognized Main item.
    pub fn main_tag(&self) -> Option<MainTag> {
        self.typed_tag(ItemType::Main, MainTag::from_code)
    }

    /// Typed Global tag, if this is a recognized Global item.
    pub fn global_tag(&self) -> Option<GlobalTag> {
        self.typed_tag(ItemType::Global, GlobalTag::from_code)
    }

    /// Typed Local tag, if this is a recognized Local item.
    pub fn local_tag(&self) -> Option<LocalTag> {
        self.typed_tag(ItemType::Local, LocalTag::from_code)
    }

    fn typed_tag<T>(&self, want: ItemType, decode: fn(u8) -> Option<T>) -> Option<T> {
        if self.long || self.item_type != want {
            return None;
        }
        decode(self.tag)
    }
}

/// Iterator over the items of a raw report descriptor.
///
/// Yields an error (and then stops) if an item header promises more bytes
/// than the buffer contains.
#[derive(Debug, Clone)]
pub struct Items<'a> {
    rest: &'a [u8],
    failed: bool,
}

impl<'a> Items<'a> {
    /// Creates an iterator over the items of a raw report descriptor.
    pub fn new(descriptor: &'a [u8]) -> Self {
        Items {
            rest: descriptor,
            failed: false,
        }
    }
}

impl<'a> Iterator for Items<'a> {
    type Item = HidResult<RawItem<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.rest.is_empty() {
            return None;
        }
        let prefix = self.rest[0];

        if prefix == 0xFE {
            // Long item: prefix, bDataSize, bLongItemTag, data.
            if self.rest.len() < 3 {
                self.failed = true;
                return Some(Err(HidError::Parse {
                    message: "truncated long item header".into(),
                }));
            }
            let size = self.rest[1] as usize;
            let tag = self.rest[2];
            if self.rest.len() < 3 + size {
                self.failed = true;
                return Some(Err(HidError::Parse {
                    message: "truncated long item payload".into(),
                }));
            }
            let data = &self.rest[3..3 + size];
            self.rest = &self.rest[3 + size..];
            return Some(Ok(RawItem {
                item_type: ItemType::Reserved,
                tag,
                data,
                long: true,
            }));
        }

        let size = match prefix & 0x03 {
            3 => 4,
            n => n as usize,
        };
        let item_type = ItemType::from_code(prefix >> 2);
        let tag = prefix >> 4;
        if self.rest.len() < 1 + size {
            self.failed = true;
            return Some(Err(HidError::Parse {
                message: "truncated short item payload".into(),
            }));
        }
        let data = &self.rest[1..1 + size];
        self.rest = &self.rest[1 + size..];
        Some(Ok(RawItem {
            item_type,
            tag,
            data,
            long: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_short_items() {
        // Usage Page (Generic Desktop), Usage (Mouse), Collection (Application)
        let bytes = [0x05, 0x01, 0x09, 0x02, 0xA1, 0x01];
        let items: Vec<_> = Items::new(&bytes).collect::<Result<_, _>>().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].global_tag(), Some(GlobalTag::UsagePage));
        assert_eq!(items[0].unsigned(), 0x01);
        assert_eq!(items[1].local_tag(), Some(LocalTag::Usage));
        assert_eq!(items[2].main_tag(), Some(MainTag::Collection));
    }

    #[test]
    fn sign_extends() {
        // Logical Minimum (-127)
        let bytes = [0x15, 0x81];
        let item = Items::new(&bytes).next().unwrap().unwrap();
        assert_eq!(item.signed(), -127);
        assert_eq!(item.unsigned(), 0x81);
    }

    #[test]
    fn four_byte_size_code() {
        // prefix size code 3 means 4 bytes
        let bytes = [0x17, 0x01, 0x00, 0x00, 0x80];
        let item = Items::new(&bytes).next().unwrap().unwrap();
        assert_eq!(item.data.len(), 4);
        assert_eq!(item.signed(), -2147483647);
    }

    #[test]
    fn tag_codes_round_trip() {
        // The macro generates `code` and `from_code` from one list; this pins
        // that they stay inverses, and that gaps stay gaps.
        for tag in [
            MainTag::Input,
            MainTag::Output,
            MainTag::Feature,
            MainTag::Collection,
            MainTag::EndCollection,
        ] {
            assert_eq!(MainTag::from_code(tag.code()), Some(tag));
        }
        for tag in [
            GlobalTag::UsagePage,
            GlobalTag::LogicalMinimum,
            GlobalTag::LogicalMaximum,
            GlobalTag::PhysicalMinimum,
            GlobalTag::PhysicalMaximum,
            GlobalTag::UnitExponent,
            GlobalTag::Unit,
            GlobalTag::ReportSize,
            GlobalTag::ReportId,
            GlobalTag::ReportCount,
            GlobalTag::Push,
            GlobalTag::Pop,
        ] {
            assert_eq!(GlobalTag::from_code(tag.code()), Some(tag));
        }
        for tag in [
            LocalTag::Usage,
            LocalTag::UsageMinimum,
            LocalTag::UsageMaximum,
            LocalTag::DesignatorIndex,
            LocalTag::DesignatorMinimum,
            LocalTag::DesignatorMaximum,
            LocalTag::StringIndex,
            LocalTag::StringMinimum,
            LocalTag::StringMaximum,
            LocalTag::Delimiter,
        ] {
            assert_eq!(LocalTag::from_code(tag.code()), Some(tag));
        }
        // 0x6 is the reserved gap between Designator Maximum and String Index.
        assert_eq!(LocalTag::from_code(0x6), None);
        assert_eq!(GlobalTag::from_code(0xC), None);
        assert_eq!(MainTag::from_code(0b1101), None);

        for ty in [
            ItemType::Main,
            ItemType::Global,
            ItemType::Local,
            ItemType::Reserved,
        ] {
            assert_eq!(ItemType::from_code(ty.code()), ty);
        }
    }

    #[test]
    fn truncated_input_errors() {
        let bytes = [0x05]; // promises 1 data byte, has none
        let mut it = Items::new(&bytes);
        assert!(it.next().unwrap().is_err());
        assert!(it.next().is_none());
    }
}
