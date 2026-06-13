//! Raw HID report descriptor item lexer.
//!
//! These are the lowest-level primitives: an iterator over the short/long
//! items of a report descriptor, plus typed tags. See Device Class Definition
//! for HID 1.11, section 6.2.2.

use crate::error::{HidError, HidResult};

/// `bType` of a short item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemType {
    Main,
    Global,
    Local,
    Reserved,
}

/// Tags of Main items (HID 1.11, 6.2.2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainTag {
    Input,
    Output,
    Feature,
    Collection,
    EndCollection,
}

/// Tags of Global items (HID 1.11, 6.2.2.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalTag {
    UsagePage,
    LogicalMinimum,
    LogicalMaximum,
    PhysicalMinimum,
    PhysicalMaximum,
    UnitExponent,
    Unit,
    ReportSize,
    ReportId,
    ReportCount,
    Push,
    Pop,
}

/// Tags of Local items (HID 1.11, 6.2.2.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalTag {
    Usage,
    UsageMinimum,
    UsageMaximum,
    DesignatorIndex,
    DesignatorMinimum,
    DesignatorMaximum,
    StringIndex,
    StringMinimum,
    StringMaximum,
    Delimiter,
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
        if self.long || self.item_type != ItemType::Main {
            return None;
        }
        Some(match self.tag {
            0b1000 => MainTag::Input,
            0b1001 => MainTag::Output,
            0b1011 => MainTag::Feature,
            0b1010 => MainTag::Collection,
            0b1100 => MainTag::EndCollection,
            _ => return None,
        })
    }

    /// Typed Global tag, if this is a recognized Global item.
    pub fn global_tag(&self) -> Option<GlobalTag> {
        if self.long || self.item_type != ItemType::Global {
            return None;
        }
        Some(match self.tag {
            0x0 => GlobalTag::UsagePage,
            0x1 => GlobalTag::LogicalMinimum,
            0x2 => GlobalTag::LogicalMaximum,
            0x3 => GlobalTag::PhysicalMinimum,
            0x4 => GlobalTag::PhysicalMaximum,
            0x5 => GlobalTag::UnitExponent,
            0x6 => GlobalTag::Unit,
            0x7 => GlobalTag::ReportSize,
            0x8 => GlobalTag::ReportId,
            0x9 => GlobalTag::ReportCount,
            0xA => GlobalTag::Push,
            0xB => GlobalTag::Pop,
            _ => return None,
        })
    }

    /// Typed Local tag, if this is a recognized Local item.
    pub fn local_tag(&self) -> Option<LocalTag> {
        if self.long || self.item_type != ItemType::Local {
            return None;
        }
        Some(match self.tag {
            0x0 => LocalTag::Usage,
            0x1 => LocalTag::UsageMinimum,
            0x2 => LocalTag::UsageMaximum,
            0x3 => LocalTag::DesignatorIndex,
            0x4 => LocalTag::DesignatorMinimum,
            0x5 => LocalTag::DesignatorMaximum,
            0x7 => LocalTag::StringIndex,
            0x8 => LocalTag::StringMinimum,
            0x9 => LocalTag::StringMaximum,
            0xA => LocalTag::Delimiter,
            _ => return None,
        })
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
        let item_type = match (prefix >> 2) & 0x03 {
            0 => ItemType::Main,
            1 => ItemType::Global,
            2 => ItemType::Local,
            _ => ItemType::Reserved,
        };
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
    fn truncated_input_errors() {
        let bytes = [0x05]; // promises 1 data byte, has none
        let mut it = Items::new(&bytes);
        assert!(it.next().unwrap().is_err());
        assert!(it.next().is_none());
    }
}
