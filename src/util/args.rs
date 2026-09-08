//! A small X11-style argument parser, generated from a struct definition.
//!
//! X servers take single-dash long options (`-display`, `-nolisten`) and stop
//! at the first non-option argument. clap can't express that, so
//! `define_config!` generates a config struct, its parser, and `-help` text
//! (formatted like `Xwayland -help`) straight from the field definitions: the
//! option name is the field name with `_` replaced with `-`, matched
//! case-insensitively, and value/flag handling follows the field's kind.
//! Everything after the first non-option argument (or a `--`) is the command to
//! run. Values may be given as `-opt value` or `-opt=value`.

/// A fixed-choice option value (an enum), for the `choice` field kind.
pub trait ArgEnum: Sized + Copy {
    /// The accepted value strings, in help-display order.
    const VARIANTS: &'static [&'static str];
    /// Parses a value string (case-insensitive); `None` if unrecognised.
    fn from_arg(s: &str) -> Option<Self>;
}

/// The long option name for a field: its name with underscores turned to dashes.
macro_rules! opt_name {
    ($field:ident) => {
        stringify!($field).replace('_', "-")
    };
}

/// The default value of a field, by kind.
macro_rules! arg_default {
    ($ty:ty, flag) => {
        false
    };
    ($ty:ty, value, $vn:literal) => {
        ::core::option::Option::None
    };
    ($ty:ty, choice, $def:literal) => {
        <$ty as $crate::util::ArgEnum>::from_arg($def).expect("invalid default")
    };
}

/// The `-option value` text shown in help, by kind.
macro_rules! arg_usage {
    ($ty:ty, flag, $field:ident) => {
        format!("-{}", opt_name!($field))
    };
    ($ty:ty, value, $field:ident, $vn:literal) => {
        format!("-{} {}", opt_name!($field), $vn)
    };
    ($ty:ty, choice, $field:ident, $def:literal) => {
        format!(
            "-{} [{}]",
            opt_name!($field),
            <$ty as $crate::util::ArgEnum>::VARIANTS.join("|")
        )
    };
}

/// Fetches an option's value: the inline `=value`, else the next argv token.
macro_rules! take_value {
    ($inline:ident, $argv:ident, $i:ident, $prog:ident, $field:ident) => {
        match $inline.take() {
            ::core::option::Option::Some(v) => v,
            ::core::option::Option::None => {
                $i += 1;
                match $argv.get($i).and_then(|a| a.to_str()) {
                    ::core::option::Option::Some(v) => v.to_string(),
                    ::core::option::Option::None => Self::usage_error(
                        &$prog,
                        &format!("option -{} requires a value", opt_name!($field)),
                    ),
                }
            }
        }
    };
}

/// Applies a parsed option to its field, by kind.
macro_rules! arg_apply {
    ($ty:ty, flag, $field:ident, $inline:ident, $argv:ident, $i:ident, $prog:ident) => {
        $field = true;
    };
    ($ty:ty, value, $field:ident, $inline:ident, $argv:ident, $i:ident, $prog:ident, $vn:literal) => {{
        let raw = take_value!($inline, $argv, $i, $prog, $field);
        match raw.parse() {
            ::core::result::Result::Ok(v) => $field = ::core::option::Option::Some(v),
            ::core::result::Result::Err(_) => Self::usage_error(
                &$prog,
                &format!("invalid value {:?} for -{}", raw, opt_name!($field)),
            ),
        }
    }};
    ($ty:ty, choice, $field:ident, $inline:ident, $argv:ident, $i:ident, $prog:ident, $def:literal) => {{
        let raw = take_value!($inline, $argv, $i, $prog, $field);
        match <$ty as $crate::util::ArgEnum>::from_arg(&raw) {
            ::core::option::Option::Some(v) => $field = v,
            ::core::option::Option::None => Self::usage_error(
                &$prog,
                &format!(
                    "invalid value {:?} for -{} (expected one of: {})",
                    raw,
                    opt_name!($field),
                    <$ty as $crate::util::ArgEnum>::VARIANTS.join(", "),
                ),
            ),
        }
    }};
}

/// Generates a configuration struct plus its X11-style parser and help text.
macro_rules! define_config {
    (
        $(#[doc = $sdoc:literal])*
        pub struct $name:ident {
            $(
                $(#[doc = $doc:literal])*
                $field:ident : $ty:ty = $kind:ident $(($karg:literal))?
            ),* $(,)?
        }
    ) => {
        #[derive(Debug)]
        pub struct $name {
            $( pub $field: $ty, )*
            /// The wrapped command and its arguments (everything after the
            /// options, or after a `--`).
            pub command: Vec<std::ffi::OsString>,
        }

        impl $name {
            /// Parses the process arguments X11-style, exiting on `-help`,
            /// `-version`, or a usage error.
            pub fn parse() -> Self {
                let mut it = std::env::args_os();
                let prog0 = it.next().unwrap_or_default();
                let prog = std::path::Path::new(&prog0)
                    .file_name()
                    .map_or_else(|| env!("CARGO_PKG_NAME").to_string(), |s| s.to_string_lossy().into_owned());
                let argv: Vec<std::ffi::OsString> = it.collect();

                $( let mut $field = arg_default!($ty, $kind $(, $karg)?); )*
                let mut command: Vec<std::ffi::OsString> = Vec::new();

                let mut i = 0;
                'next: while i < argv.len() {
                    // Non-UTF-8 can't be one of our options; treat as command.
                    let Some(s) = argv[i].to_str() else { break };
                    if s == "--" {
                        i += 1;
                        break;
                    }
                    // First non-option argument: the command starts here.
                    let body = match s.strip_prefix('-') {
                        Some(b) if !b.is_empty() => b,
                        _ => break,
                    };
                    let (name, mut inline) = match body.split_once('=') {
                        Some((n, v)) => (n.to_ascii_lowercase(), Some(v.to_string())),
                        None => (body.to_ascii_lowercase(), None),
                    };
                    if name == "help" || name == "h" {
                        print!("{}", Self::help(&prog));
                        std::process::exit(0);
                    }
                    if name == "version" || name == "v" {
                        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                        println!("{}", env!("CARGO_PKG_REPOSITORY"));
                        std::process::exit(0);
                    }
                    $(
                        if name == opt_name!($field) {
                            arg_apply!($ty, $kind, $field, inline, argv, i, prog $(, $karg)?);
                            let _ = &mut inline;
                            i += 1;
                            continue 'next;
                        }
                    )*
                    Self::usage_error(&prog, &format!("unknown option -{name}"));
                }
                command.extend_from_slice(&argv[i..]);
                Self { $( $field, )* command }
            }

            fn usage_error(prog: &str, msg: &str) -> ! {
                eprintln!("{prog}: {msg}");
                eprintln!("Try '{prog} -help' for more information.");
                std::process::exit(2);
            }

            /// `Xwayland -help`-style usage text.
            fn help(prog: &str) -> String {
                let mut rows: Vec<(String, String)> = vec![
                    $((
                        arg_usage!($ty, $kind, $field $(, $karg)?),
                        [$($doc.trim()),*].join(" "),
                    ),)*
                ];
                rows.push(("-help".to_string(), "print this message".to_string()));
                rows.push(("-version".to_string(), "show the version and exit".to_string()));

                let mut out = format!("use: {prog} [options] command [args ...]\n");
                for (opt, desc) in rows {
                    let pad = 45usize.saturating_sub(opt.len()).max(1);
                    out.push_str(&format!("{opt}{}{desc}\n", " ".repeat(pad)));
                }
                out
            }
        }
    };
}
