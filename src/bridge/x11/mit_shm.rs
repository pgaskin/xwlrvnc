use std::collections::HashMap;

/// Manages SysV memory clients from the client via MIT-SHM.
#[derive(Default)]
pub(super) struct Shm {
    segments: HashMap<u32, ShmSeg>,
}

pub(super) struct ShmSeg {
    pub ptr: *mut u8,
    pub size: usize,
}

impl Shm {
    pub fn attach(&mut self, shmseg: u32, shmid: u32) {
        let ptr = unsafe { libc::shmat(shmid as i32, std::ptr::null(), 0) };
        if std::ptr::eq(ptr, libc::MAP_FAILED) {
            crate::warning!("shmat failed for shmid {shmid}");
            return;
        }
        let size = unsafe {
            let mut ds: libc::shmid_ds = std::mem::zeroed();
            if libc::shmctl(shmid as i32, libc::IPC_STAT, &mut ds) == 0 {
                ds.shm_segsz as usize
            } else {
                0
            }
        };
        self.segments.insert(
            shmseg,
            ShmSeg {
                ptr: ptr.cast(),
                size,
            },
        );
    }

    pub fn detach(&mut self, shmseg: u32) {
        if let Some(seg) = self.segments.remove(&shmseg) {
            unsafe { libc::shmdt(seg.ptr.cast()) };
        }
    }

    pub fn segment(&self, shmseg: u32) -> Option<&ShmSeg> {
        self.segments.get(&shmseg)
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        for (_, seg) in self.segments.drain() {
            unsafe { libc::shmdt(seg.ptr.cast()) };
        }
    }
}
