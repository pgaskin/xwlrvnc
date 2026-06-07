//! The set of X11 extensions we pretend to support, with the major opcodes and
//! event/error bases we assign them.
//!
//! The names must match x11rb-protocol's `X11_EXTENSION_NAME` constants exactly,
//! since [`x11rb_protocol::protocol::Request::parse`] dispatches extension
//! requests by looking the major opcode up here and matching the returned name.

use x11rb_protocol::x11_utils::{ExtInfoProvider, ExtensionInformation};

pub struct ExtDef {
    pub name: &'static str,
    pub major_opcode: u8,
    pub first_event: u8,
    pub first_error: u8,
}

/// Event/error bases are assigned by hand into the extension ranges (events
/// >= 64, errors >= 128) and just need to be internally consistent and echoed
/// back in `QueryExtension`.
pub static EXTENSIONS: &[ExtDef] = &[
    ExtDef { name: "BIG-REQUESTS", major_opcode: 134, first_event: 0, first_error: 0 },
    ExtDef { name: "MIT-SHM", major_opcode: 130, first_event: 64, first_error: 128 },
    ExtDef { name: "XTEST", major_opcode: 131, first_event: 0, first_error: 0 },
    ExtDef { name: "RANDR", major_opcode: 132, first_event: 65, first_error: 129 },
    ExtDef { name: "DAMAGE", major_opcode: 133, first_event: 67, first_error: 133 },
    ExtDef { name: "XFIXES", major_opcode: 135, first_event: 68, first_error: 134 },
];

/// Looks up an extension by name (case-insensitive, as `QueryExtension` allows).
pub fn lookup(name: &[u8]) -> Option<&'static ExtDef> {
    EXTENSIONS
        .iter()
        .find(|e| e.name.as_bytes().eq_ignore_ascii_case(name))
}

/// Provides extension info to x11rb's request parser. Only `get_from_major_opcode`
/// is consulted when parsing requests; the event/error lookups are for parsing
/// incoming events, which a server never receives.
pub struct ExtInfo;

impl ExtInfoProvider for ExtInfo {
    fn get_from_major_opcode(&self, major_opcode: u8) -> Option<(&str, ExtensionInformation)> {
        EXTENSIONS
            .iter()
            .find(|e| e.major_opcode == major_opcode)
            .map(|e| {
                (
                    e.name,
                    ExtensionInformation {
                        major_opcode: e.major_opcode,
                        first_event: e.first_event,
                        first_error: e.first_error,
                    },
                )
            })
    }

    fn get_from_event_code(&self, _event_code: u8) -> Option<(&str, ExtensionInformation)> {
        None
    }

    fn get_from_error_code(&self, _error_code: u8) -> Option<(&str, ExtensionInformation)> {
        None
    }
}
