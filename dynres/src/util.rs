pub struct Stderr;

// note: I originally tried to use claude to write these helpers, but it was
// absolutely terrible at it, generating a lot of c-style code without error
// checking which didn't even compile...

impl core::fmt::Write for Stderr {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // stderr() is `unsafe` in no_std (no guarantee the fd is valid at the
        // call site; in an LD_PRELOAD constructor fd 2 is always open).
        let fd = unsafe { rustix::stdio::stderr() };
        let b = s.as_bytes();
        let mut pos = 0;
        while pos < b.len() {
            match rustix::io::write(fd, &b[pos..]) {
                Ok(n) if n > 0 => pos += n,
                _ => break,
            }
        }
        Ok(())
    }
}

pub struct Environ<'a> {
    data: &'a [u8],
}

impl<'a> Environ<'a> {
    pub fn read(buf: &'a mut [u8]) -> Result<Self, rustix::io::Errno> {
        Ok(Self {
            data: read_file(c"/proc/self/environ", buf)?,
        })
    }

    pub fn get(&self, name: &[u8]) -> Option<&'a [u8]> {
        self.data
            .split(|&b| b == 0)
            .filter_map(|entry| {
                let eq = entry.iter().position(|&b| b == b'=')?;
                Some((&entry[..eq], &entry[eq + 1..]))
            })
            .find_map(|(k, v)| (k == name).then_some(v))
    }
}

pub struct Mappings<'a> {
    data: &'a mut [Mapping],
    len: usize,
}

impl<'a> Mappings<'a> {
    pub fn read(buf: &'a mut [Mapping], file_buf: &mut [u8]) -> Result<Self, rustix::io::Errno> {
        let data = read_file(c"/proc/self/maps", file_buf)?;
        let mut maps = Self { data: buf, len: 0 };
        for line in data.split(|&b| b == b'\n') {
            if let Some(map) = Mapping::parse(line) {
                if maps.len < maps.data.len() {
                    maps.data[maps.len] = map;
                    maps.len += 1;
                }
            }
        }
        Ok(maps)
    }

    pub fn file_offset(&self, addr: usize) -> Option<usize> {
        self.iter()
            .find(|m| addr >= m.lo && addr < m.hi)
            .map(|m| m.file_offset(addr))
    }

    pub fn find_mapping(&self, addr: usize, only_executable: bool) -> Option<&Mapping> {
        self.iter()
            .find(|m| (!only_executable || m.ex) && addr >= m.lo && addr < m.hi)
    }

    pub unsafe fn find_bytes(&self, pattern: &[u8], not_in_map_of: usize) -> Option<usize> {
        for map in self.iter().filter(|s| s.private && s.file_backed) {
            if not_in_map_of >= map.lo && not_in_map_of < map.hi {
                continue;
            }
            let data = core::slice::from_raw_parts(map.lo as *const u8, map.hi - map.lo);
            if let Some(i) = data.windows(pattern.len()).position(|w| w == pattern) {
                return Some(map.lo + i);
            }
        }
        None
    }
}

impl core::ops::Deref for Mappings<'_> {
    type Target = [Mapping];
    fn deref(&self) -> &[Mapping] {
        &self.data[..self.len]
    }
}

#[derive(Clone, Copy, Default)]
pub struct Mapping {
    pub lo: usize,
    pub hi: usize,
    pub rd: bool,
    pub wr: bool,
    pub ex: bool,
    pub offset: usize,
    // Only MAP_PRIVATE segments are scanned. MAP_SHARED segments backed by
    // device files (e.g. GPU/DRI memory) appear readable but SIGBUS on access.
    pub private: bool,
    // Only file-backed segments (device != 00:00) are scanned. Anonymous and
    // special kernel mappings like [vvar] appear readable but SIGBUS on access;
    // RTTI strings and typeinfo/vtable structs are always in file-backed sections
    // (.rodata, .data.rel.ro), so nothing of interest lives in anonymous pages.
    pub file_backed: bool,
}

impl Mapping {
    fn parse(line: &[u8]) -> Option<Self> {
        let mut p = 0;
        let lo = parse_hex(line, &mut p)?;
        if *line.get(p)? != b'-' {
            return None;
        }
        p += 1;
        let hi = parse_hex(line, &mut p)?;
        if *line.get(p)? != b' ' {
            return None;
        }
        p += 1;
        if p + 4 > line.len() {
            return None;
        }
        let readable = line[p] == b'r';
        let writable = line[p + 1] == b'w';
        let executable = line[p + 2] == b'x';
        let private = line[p + 3] == b'p';
        if !readable {
            return None;
        }
        p += 4;
        if *line.get(p)? != b' ' {
            return None;
        }
        p += 1;
        let offset = parse_hex(line, &mut p).unwrap_or(0);
        if *line.get(p)? != b' ' {
            return None;
        }
        p += 1;
        let major = parse_hex(line, &mut p).unwrap_or(0);
        let minor = if line.get(p).copied() == Some(b':') {
            p += 1;
            parse_hex(line, &mut p).unwrap_or(0)
        } else {
            0
        };
        let file_backed = major != 0 || minor != 0;
        Some(Self {
            lo,
            hi,
            rd: readable,
            wr: writable,
            ex: executable,
            offset,
            private,
            file_backed,
        })
    }

    pub fn file_offset(&self, addr: usize) -> usize {
        self.offset + (addr - self.lo)
    }

    pub unsafe fn writable_page(&self, addr: *mut u8, flush_len: usize) -> WritablePage {
        WritablePage::new(addr, flush_len, self)
    }

    fn flags(&self) -> rustix::mm::MprotectFlags {
        let mut flags = rustix::mm::MprotectFlags::empty();
        if self.rd {
            flags |= rustix::mm::MprotectFlags::READ;
        }
        if self.wr {
            flags |= rustix::mm::MprotectFlags::WRITE;
        }
        if self.ex {
            flags |= rustix::mm::MprotectFlags::EXEC;
        }
        flags
    }
}
pub struct WritablePage {
    page: *mut core::ffi::c_void,
    pgsz: usize,
    addr: *mut u8,
    flush: usize,
    flags: rustix::mm::MprotectFlags,
}

impl WritablePage {
    unsafe fn new(addr: *mut u8, flush: usize, mapping: &Mapping) -> Self {
        let pgsz = rustix::param::page_size();
        let page = (addr as usize & !(pgsz - 1)) as *mut _;
        let flags = mapping.flags();
        let _ = rustix::mm::mprotect(
            page,
            pgsz,
            flags | rustix::mm::MprotectFlags::WRITE,
        );
        Self {
            page,
            pgsz,
            addr,
            flush,
            flags,
        }
    }

    pub unsafe fn write(&self, addr: *mut u8, src: &[u8]) {
        for (i, &byte) in src.iter().enumerate() {
            unsafe { addr.add(i).write_volatile(byte) }
        }
    }
}

impl Drop for WritablePage {
    fn drop(&mut self) {
        unsafe {
            flush_icache(self.addr, self.flush);
            let _ = rustix::mm::mprotect(self.page, self.pgsz, self.flags);
        }
    }
}

unsafe fn flush_icache(start: *mut u8, len: usize) {
    #[cfg(target_arch = "x86_64")]
    let _ = (start, len); // no-op

    #[cfg(target_arch = "aarch64")]
    core::arch::asm!(
        "dc  cvau, {addr}", // clean D-cache line containing len (for small-ish len value)
        "dsb ish",          // data sync barrier
        "ic  ivau, {addr}", // invalidate I-cache containing len (for small-ish len value)
        "dsb ish",          // data sync barrier
        "isb",              // instruction sync barrier
        addr = in(reg) start,
        options(nostack, preserves_flags),
    );

    #[cfg(target_arch = "arm")]
    core::arch::asm!(
        "push {{r7}}",
        "mov  r7, {nr}",
        "svc  #0",
        "pop  {{r7}}",
        nr      = in(reg) 0xf0002_u32, // cacheflush
        inout("r0") start as u32 => _,
        in("r1")    start.add(len) as u32,
        in("r2")    0_u32,
        options(nostack),
    );
}

pub fn untag(ptr: usize) -> usize {
    #[cfg(target_arch = "arm")]
    return ptr & !1usize; // thumb bit

    #[cfg(target_arch = "aarch64")]
    return ptr & !(0xFFusize << 56); // tbi

    ptr
}

pub unsafe fn read_ptr(addr: usize) -> usize {
    (untag(addr) as *const usize).read_unaligned()
}

fn read_file<'a>(path: &core::ffi::CStr, buf: &'a mut [u8]) -> Result<&'a [u8], rustix::io::Errno> {
    let fd = rustix::fs::openat(
        rustix::fs::CWD,
        path,
        rustix::fs::OFlags::RDONLY,
        rustix::fs::Mode::empty(),
    )?;
    let mut total = 0;
    while total < buf.len() {
        match rustix::io::read(&fd, &mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) => return Err(e),
        }
    }
    Ok(&buf[..total])
}

pub fn parse_hex(buf: &[u8], pos: &mut usize) -> Option<usize> {
    let start = *pos;
    let mut val = 0usize;
    while *pos < buf.len() {
        let d = match buf[*pos] {
            b'0'..=b'9' => buf[*pos] - b'0',
            b'a'..=b'f' => buf[*pos] - b'a' + 10,
            b'A'..=b'F' => buf[*pos] - b'A' + 10,
            _ => break,
        };
        val = val.wrapping_mul(16).wrapping_add(d as usize);
        *pos += 1;
    }
    if *pos > start {
        Some(val)
    } else {
        None
    }
}

pub fn parse_bool(s: &[u8]) -> Option<bool> {
    let mut lower = [0u8; 5]; // max length of known values + 1
    let s = s.get(..lower.len()).unwrap_or(s);
    let lower = {
        let l = &mut lower[..s.len()];
        l.copy_from_slice(s);
        l.make_ascii_lowercase();
        l as &[u8]
    };
    match lower {
        b"1" | b"true" | b"yes" => Some(true),
        b"0" | b"false" | b"no" => Some(false),
        _ => None,
    }
}
