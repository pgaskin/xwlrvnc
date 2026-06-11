use x11rb_protocol::{
    protocol,
    x11_utils::{ExtInfoProvider, ExtensionInformation},
};

pub struct ExtDef {
    /// Must match `x11rb_protocol::protocol::*::X11_EXTENSION_NAME` for it to
    /// work with [`x11rb_protocol::protocol::Request::parse`].
    pub name: &'static str,
    /// Arbitrary, but unique for the server, and typically 128..255.
    pub major_opcode: u8,
    /// Arbitrary, but unique for the server, and typically 64..127. There must be enough
    /// room for all extension events to not overlap. Can be 0 if none.
    pub first_event: u8,
    /// Arbitrary, but unique for the server. and typically 128..255. There must
    /// be enough room for all extension errors to not overlap. Can be 0 if none.
    pub first_error: u8,
}

pub struct Extensions(pub &'static [ExtDef]);

pub static EXTENSIONS: Extensions = Extensions(&[
    // this is a bit wasteful with events/errors, but we have room, so...
    ExtDef {
        name: protocol::shm::X11_EXTENSION_NAME,
        major_opcode: 130,
        first_event: 65,
        first_error: 130,
    },
    ExtDef {
        name: protocol::xtest::X11_EXTENSION_NAME,
        major_opcode: 131,
        first_event: 70,
        first_error: 135,
    },
    ExtDef {
        name: protocol::randr::X11_EXTENSION_NAME,
        major_opcode: 132,
        first_event: 75,
        first_error: 140,
    },
    ExtDef {
        name: protocol::damage::X11_EXTENSION_NAME,
        major_opcode: 133,
        first_event: 80,
        first_error: 145,
    },
    ExtDef {
        name: protocol::bigreq::X11_EXTENSION_NAME,
        major_opcode: 134,
        first_event: 85,
        first_error: 150,
    },
    ExtDef {
        name: protocol::xfixes::X11_EXTENSION_NAME,
        major_opcode: 135,
        first_event: 90,
        first_error: 155,
    },
]);

impl Extensions {
    /// Look up an extension by name (case-insensitive like `QueryExtension`).
    pub fn lookup(&self, name: &[u8]) -> Option<&'static ExtDef> {
        self.0
            .iter()
            .find(|e| e.name.as_bytes().eq_ignore_ascii_case(name))
    }
}

impl ExtInfoProvider for Extensions {
    fn get_from_major_opcode(&self, major_opcode: u8) -> Option<(&str, ExtensionInformation)> {
        self.0
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
        None // server does not receive events
    }

    fn get_from_error_code(&self, _error_code: u8) -> Option<(&str, ExtensionInformation)> {
        None // server does not receive errors
    }
}
