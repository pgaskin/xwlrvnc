#![no_std]
#![no_builtins]

// note: memcmp and memcpy will still be imported as undefined symbols

mod util;
use core::fmt::Write as _;
use util::*;

#[cfg(not(test))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop()
    }
}

#[used]
#[link_section = ".init_array"]
static CTOR: unsafe extern "C" fn() = {
    unsafe extern "C" fn init() {
        unsafe {
            patch();
        }
    }
    init
};

macro_rules! log {
    ($($arg:tt)*) => {{
        let mut w = Stderr;
        let _ = write!(w, "rvnc_dynres_hook: ");
        let _ = write!(w, $($arg)*);
        let _ = write!(w, "\n");
    }};
}

const MAX_READ_BYTES: usize = 24280; // for environ and maps
const MAX_MAPS_ENTRIES: usize = 1024;
const MAX_VTABLE_SLOTS: usize = 16;
const FN_SCAN_LIMIT: usize = 64;

const RTTI_NAME: &[u8] = b"N5smisc25SFeatureDynamicResolutionE\0";

unsafe fn patch() {
    let mut buf = [0u8; MAX_READ_BYTES];

    // handle env vars
    {
        let env = match Environ::read(&mut buf) {
            Ok(e) => e,
            Err(e) => {
                log!("failed to read /proc/self/environ: {e}");
                return;
            }
        };
        if !parse_bool(env.get(b"FORCE_DYNRES").unwrap_or(b"0")).unwrap_or(false) {
            return;
        }
    }

    // read proc maps
    let mut maps_arr = [Mapping::default(); MAX_MAPS_ENTRIES];
    let maps = match Mappings::read(&mut maps_arr, &mut buf) {
        Ok(m) => m,
        Err(e) => {
            log!("failed to read /proc/self/maps: {e}");
            return;
        }
    };
    if maps.is_empty() {
        return;
    }
    let ptr = core::mem::size_of::<usize>();

    // find the typeinfo name pointer target, skipping our own copy in the hook's .rodata
    let name_addr = match maps.find_bytes(RTTI_NAME, RTTI_NAME.as_ptr() as usize) {
        Some(a) => a,
        None => return, // missing or we don't care about the process
    };
    log!(
        "found '{}' {name_addr:#x} (@{:#x})",
        core::str::from_utf8(&RTTI_NAME[..RTTI_NAME.len() - 1]).unwrap_or("?"),
        maps.file_offset(name_addr).unwrap_or(0)
    );

    // find the typeinfo pointing to it
    let typeinfo_addr = match find_typeinfo(&maps, name_addr) {
        Some(a) => a,
        None => {
            log!("error: rtti not found");
            return;
        }
    };
    log!(
        "found rtti {typeinfo_addr:#x} (@{:#x})",
        maps.file_offset(typeinfo_addr).unwrap_or(0)
    );

    // find the vtable for the type
    let vtable_addr = match find_vtable(&maps, typeinfo_addr) {
        Some(a) => a,
        None => {
            log!("error: vtable not found");
            return;
        }
    };
    log!(
        "found vtable {vtable_addr:#x} (@{:#x})",
        maps.file_offset(vtable_addr).unwrap_or(0)
    );

    // find the vtable slot for the function which checks if the module is
    // enabled and returns if "-virtual" is not specified, then patch it
    for slot in 0..MAX_VTABLE_SLOTS {
        let slot_addr = vtable_addr + slot * ptr;
        if !maps
            .iter()
            .any(|s| slot_addr >= s.lo && slot_addr + ptr <= s.hi)
        {
            break;
        }
        let fn_ptr = read_ptr(slot_addr);

        // we don't implement thumb patching, and it's not thumb anyways
        #[cfg(target_arch = "arm")]
        if fn_ptr & 1 != 0 {
            continue;
        }

        // clean the pointer
        let fn_addr = util::untag(fn_ptr);

        // don't go off the end
        let map = match maps.find_mapping(fn_addr, true) {
            None => break,
            Some(map) => map,
        };

        // look for the check and patch it
        let insts = core::slice::from_raw_parts(
            fn_addr as *const u8,
            (map.hi - fn_addr).min(FN_SCAN_LIMIT),
        );
        if let Some(k) = virtual_check::find(insts) {
            log!(
                "patching '-virtual' check in slot {slot} fn {fn_addr:#x}+{k} (@{:#x})",
                maps.file_offset(fn_addr + k).unwrap_or(0)
            );
            let addr = (fn_addr + k) as *mut u8;
            let wrpg = map.writable_page(addr, 16);
            virtual_check::patch(&wrpg, addr);
            log!("success");
            return;
        }
    }
    log!("error: '-virtual' check not found in any vtable slot");
}

unsafe fn find_typeinfo(maps: &Mappings, name_addr: usize) -> Option<usize> {
    let ptr = core::mem::size_of::<usize>();
    for map in maps.iter().filter(|s| s.private && s.file_backed) {
        let count = (map.hi - map.lo) / ptr;
        for j in 1..count {
            let addr = map.lo + j * ptr;
            if read_ptr(addr) == name_addr {
                let vptr = read_ptr(addr - ptr);
                if vptr != 0 && vptr & 7 == 0 {
                    return Some(addr - ptr);
                }
            }
        }
    }
    None
}

unsafe fn find_vtable(maps: &Mappings, typeinfo_addr: usize) -> Option<usize> {
    let ptr = core::mem::size_of::<usize>();
    for map in maps.iter().filter(|s| s.private && s.file_backed) {
        let count = (map.hi - map.lo) / ptr;
        for j in 0..count.saturating_sub(1) {
            let addr = map.lo + j * ptr;
            if read_ptr(addr) == typeinfo_addr {
                // validate that the first function pointer in it is executable
                // to avoid false-positives
                let fn0 = read_ptr(addr + ptr);
                if maps.find_mapping(fn0, false).is_some() {
                    return Some(addr + ptr);
                }
            }
        }
    }
    None
}

#[cfg(target_arch = "x86_64")]
mod virtual_check {
    pub fn find(insts: &[u8]) -> Option<usize> {
        for k in 0..insts.len().saturating_sub(2) {
            if insts[k] & 0xf0 == 0x70 && insts[k + 1] > 0 && insts[k + 2] == 0xc3 {
                // Jcc short +N (7x); RET (c3)
                return Some(k);
            }
        }
        None
    }
    pub unsafe fn patch(page: &super::WritablePage, tgt: *mut u8) {
        page.write(tgt, &[0xeb]); // jmp short rel8
    }
}

#[cfg(target_arch = "aarch64")]
mod virtual_check {
    pub fn find(insts: &[u8]) -> Option<usize> {
        let steps = insts.len() / 4;
        for s in 0..steps.saturating_sub(2) {
            let k = s * 4;
            if insts[k + 3] == 0x54 // B.cond opcode
                && (insts[k] & 0x0f) != 0x0e // cond != AL (0xe)
                && insts[k + 4..k + 8] == [0x00, 0x00, 0x80, 0x52] // MOVZ W0, #0
                && insts[k + 8..k + 12] == [0xc0, 0x03, 0x5f, 0xd6]
            // RET (X30)
            {
                return Some(k);
            }
        }
        None
    }
    pub unsafe fn patch(page: &super::WritablePage, tgt: *mut u8) {
        page.write(tgt, &[(*tgt & 0xf0) | 0x0e]); // force cond to AL (0xe)
    }
}

#[cfg(target_arch = "arm")]
mod virtual_check {
    pub fn find(insts: &[u8]) -> Option<usize> {
        let steps = insts.len() / 4;
        for s in 0..steps.saturating_sub(3) {
            let k = s * 4;
            if insts[k] == 0x02 // CMP Rn, #2: imm8
                && insts[k + 1] == 0x00 // rot = 0, Rd = 0
                && insts[k + 2] & 0xf0 == 0x50 // CMP opcode (1010), S = 1
                && insts[k + 3] == 0xe3 // cond AL (0xe), I = 1
                && insts[k + 8..k + 12] == [0x00, 0x00, 0xa0, 0x13] // MOVNE R0, #0
                && insts[k + 12..k + 16] == [0x1e, 0xff, 0x2f, 0xe1]
            // BX LR
            {
                return Some(k);
            }
        }
        None
    }
    pub unsafe fn patch(page: &super::WritablePage, tgt: *mut u8) {
        let rn = *tgt.add(2) & 0x0f;
        page.write(tgt, &[rn, *tgt.add(1), *tgt.add(2), 0xe1]); // 0xe1: cond AL, I=0 -> CMP Rn, Rn
    }
}
