//! The X targets a Wayland-backed selection converts to, in one table, so
//! what `TARGETS` advertises and what a conversion produces cannot disagree.
//!
//! Only text is translated by name; any other mime type is offered as a
//! target under its own name and served raw. `MULTIPLE`, which the ICCCM
//! requires of owners, is deliberately not supported: no Xlib requestor we
//! know of uses it, and RealVNC's strings do not mention it. Nor is
//! `COMPOUND_TEXT`.

/// A named text target: the property type it is delivered as, and the mime
/// types that can stand in for it, most faithful first.
struct TextTarget {
    name: &'static [u8],
    /// The ICCCM says `TEXT` "is not defined as a type; it will never be the
    /// returned type", so it is delivered as the encoding we actually hold.
    type_: &'static [u8],
    mimes: &'static [&'static str],
    /// `STRING` is ISO Latin-1 by definition, and Wayland text is UTF-8.
    latin1: bool,
}

/// Mime types that are UTF-8 text, however they are labelled: the names are
/// what other X bridges (and our own sources) put on the Wayland side.
const UTF8_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
];

const TEXT_TARGETS: &[TextTarget] = &[
    TextTarget {
        name: b"UTF8_STRING",
        type_: b"UTF8_STRING",
        mimes: UTF8_MIMES,
        latin1: false,
    },
    TextTarget {
        name: b"TEXT",
        type_: b"UTF8_STRING",
        mimes: UTF8_MIMES,
        latin1: false,
    },
    TextTarget {
        name: b"STRING",
        type_: b"STRING",
        mimes: UTF8_MIMES,
        latin1: true,
    },
];

/// How to satisfy one conversion request.
#[derive(Debug, PartialEq, Eq)]
pub struct Conversion {
    /// The mime type to ask Wayland for.
    pub mime: String,
    /// The property type to deliver it as; `None` for the target atom itself.
    pub type_: Option<&'static [u8]>,
    /// Whether the (UTF-8) bytes must be transcoded to Latin-1 first.
    pub latin1: bool,
}

/// Picks the mime type and delivery for `target` from what the selection
/// offers, or `None` if it cannot be converted.
pub fn resolve(target: &[u8], mimes: &[String]) -> Option<Conversion> {
    if let Some(t) = TEXT_TARGETS.iter().find(|t| t.name == target) {
        let mime = t
            .mimes
            .iter()
            .find(|m| mimes.iter().any(|x| x == *m))
            .map(|m| (*m).to_string())
            // any other text/* is better than nothing
            .or_else(|| mimes.iter().find(|m| m.starts_with("text/")).cloned())?;
        return Some(Conversion {
            mime,
            type_: Some(t.type_),
            latin1: t.latin1,
        });
    }
    // a mime type used directly as the target
    if target.contains(&b'/') {
        let mime = std::str::from_utf8(target).ok()?;
        if mimes.iter().any(|x| x == mime) {
            return Some(Conversion {
                mime: mime.to_string(),
                type_: None,
                latin1: false,
            });
        }
    }
    None
}

/// The target names to list for `TARGETS`, beyond `TARGETS` and `TIMESTAMP`
/// themselves: exactly those [`resolve`] would accept, each once.
pub fn advertised(mimes: &[String]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for t in TEXT_TARGETS {
        if resolve(t.name, mimes).is_some() {
            out.push(t.name.to_vec());
        }
    }
    for m in mimes {
        if m.contains('/') && !out.iter().any(|o| o == m.as_bytes()) {
            out.push(m.as_bytes().to_vec());
        }
    }
    out
}

/// UTF-8 to ISO Latin-1, with `?` for what does not fit (and for bytes that
/// were not UTF-8 to begin with).
pub fn to_latin1(utf8: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(utf8)
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|m| (*m).to_string()).collect()
    }

    #[test]
    fn utf8_string_prefers_the_charset_labelled_mime() {
        let m = mimes(&["text/plain", "text/plain;charset=utf-8", "text/html"]);
        let c = resolve(b"UTF8_STRING", &m).unwrap();
        assert_eq!(c.mime, "text/plain;charset=utf-8");
        assert_eq!(c.type_, Some(&b"UTF8_STRING"[..]));
        assert!(!c.latin1);
    }

    #[test]
    fn text_is_delivered_as_a_real_type() {
        let c = resolve(b"TEXT", &mimes(&["text/plain"])).unwrap();
        assert_eq!(c.type_, Some(&b"UTF8_STRING"[..]));
    }

    #[test]
    fn string_is_transcoded() {
        let c = resolve(b"STRING", &mimes(&["text/plain"])).unwrap();
        assert_eq!(c.type_, Some(&b"STRING"[..]));
        assert!(c.latin1);
        assert_eq!(
            to_latin1("caf\u{e9} \u{2014} \u{1f600}".as_bytes()),
            b"caf\xe9 ? ?"
        );
        assert_eq!(to_latin1(b"bad \xff byte"), b"bad ? byte");
    }

    #[test]
    fn a_literal_x_name_offered_as_a_mime_is_accepted() {
        // wl-copy --type UTF8_STRING, or another bridge's source
        let c = resolve(b"UTF8_STRING", &mimes(&["UTF8_STRING"])).unwrap();
        assert_eq!(c.mime, "UTF8_STRING");
        let c = resolve(b"STRING", &mimes(&["TEXT"])).unwrap();
        assert_eq!(c.mime, "TEXT");
    }

    #[test]
    fn any_text_mime_will_do_as_a_last_resort() {
        let c = resolve(b"UTF8_STRING", &mimes(&["image/png", "text/html"])).unwrap();
        assert_eq!(c.mime, "text/html");
    }

    #[test]
    fn a_raw_mime_target_is_served_as_itself() {
        let m = mimes(&["image/png", "text/plain"]);
        let c = resolve(b"image/png", &m).unwrap();
        assert_eq!(c.mime, "image/png");
        assert_eq!(c.type_, None);
        assert_eq!(resolve(b"image/jpeg", &m), None);
        assert_eq!(resolve(b"MULTIPLE", &m), None, "deliberately unsupported");
        assert_eq!(resolve(b"UTF8_STRING", &mimes(&["image/png"])), None);
    }

    #[test]
    fn targets_advertises_exactly_what_converts_once_each() {
        // our own sources label the text five ways; each X name appears once
        let m = mimes(crate::bridge::clipboard::TEXT_MIMES);
        let adv = advertised(&m);
        assert_eq!(
            adv,
            [
                b"UTF8_STRING".to_vec(),
                b"TEXT".to_vec(),
                b"STRING".to_vec(),
                b"text/plain;charset=utf-8".to_vec(),
                b"text/plain".to_vec(),
            ]
        );
        for name in &adv {
            assert!(
                resolve(name, &m).is_some(),
                "{:?} advertised but refused",
                String::from_utf8_lossy(name)
            );
        }
        // no text at all: only the raw mime
        assert_eq!(advertised(&mimes(&["image/png"])), [b"image/png".to_vec()]);
    }
}
