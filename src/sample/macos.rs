//! macOS readings (design 9). None of them needs root; processes of other
//! users read as `None`.

use std::ffi::CString;
use std::mem::{MaybeUninit, size_of, zeroed};

use super::{Memory, System};

fn sysctl<T: Copy>(name: &str) -> Option<T> {
    let c = CString::new(name).ok()?;
    let mut v = MaybeUninit::<T>::zeroed();
    let mut len = size_of::<T>();
    // SAFETY: sysctlbyname writes at most `len` bytes into `v`.
    let r = unsafe {
        libc::sysctlbyname(
            c.as_ptr(),
            v.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    // SAFETY: on success the value was written.
    (r == 0 && len == size_of::<T>()).then(|| unsafe { v.assume_init() })
}

/// The kernel's page size, which page counts use. `hw.pagesize` gives the
/// page size of this process instead: 4 KB under Rosetta on 16 KB kernels.
fn page_size() -> u64 {
    sysctl::<u64>("vm.pagesize").unwrap_or(16384)
}

pub fn system() -> Option<System> {
    let ps = page_size();
    // SAFETY: plain data, filled by host_statistics64.
    let mut vm: libc::vm_statistics64 = unsafe { zeroed() };
    let mut count = (size_of::<libc::vm_statistics64>() / size_of::<i32>()) as u32;
    // SAFETY: `count` is the size of `vm` in integers.
    #[allow(deprecated)]
    let kr = unsafe {
        libc::host_statistics64(
            libc::mach_host_self(),
            libc::HOST_VM_INFO64,
            (&mut vm as *mut libc::vm_statistics64).cast(),
            &mut count,
        )
    };
    if kr != 0 {
        return None;
    }
    let pages = |n: u64| n * ps;
    let app = (vm.internal_page_count as u64).saturating_sub(vm.purgeable_count as u64);
    Some(System {
        mem_total: sysctl::<u64>("hw.memsize")?,
        // Activity Monitor's "memory used": app memory, wired and compressed.
        mem_used: pages(app + vm.wire_count as u64 + vm.compressor_page_count as u64),
        pressure: sysctl::<i32>("kern.memorystatus_vm_pressure_level").unwrap_or(0) as u8,
        swap_out_bytes: pages(vm.swapouts),
        cores: super::cores(),
    })
}

const PROC_PIDREGIONPATHINFO: i32 = 8;
// Share modes from <mach/vm_region.h> that are shared with other processes.
const SM_SHARED: u32 = 4;
const SM_TRUESHARED: u32 = 5;
const SM_SHARED_ALIASED: u32 = 7;

#[repr(C)]
#[derive(Clone, Copy)]
struct RegionInfo {
    protection: u32,
    max_protection: u32,
    inheritance: u32,
    flags: u32,
    offset: u64,
    behavior: u32,
    user_wired_count: u32,
    user_tag: u32,
    pages_resident: u32,
    pages_shared_now_private: u32,
    pages_swapped_out: u32,
    pages_dirtied: u32,
    ref_count: u32,
    shadow_depth: u32,
    share_mode: u32,
    private_pages_resident: u32,
    shared_pages_resident: u32,
    obj_id: u32,
    depth: u32,
    address: u64,
    size: u64,
}

#[repr(C)]
struct RegionWithPath {
    info: RegionInfo,
    vnode_info: [u8; 152],
    path: [u8; 1024],
}

/// Walks the process's memory regions. Private anonymous memory is what
/// admission counts (design 10.1); shared and file-backed pages are left
/// out, so shared memory is not counted once per process.
pub fn memory(pid: i32) -> Option<Memory> {
    let ps = page_size();
    let mut m = Memory::default();
    let mut address = 0u64;
    let mut regions = 0;
    loop {
        let mut rp = MaybeUninit::<RegionWithPath>::zeroed();
        // SAFETY: writes at most the struct's size.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                PROC_PIDREGIONPATHINFO,
                address,
                rp.as_mut_ptr().cast(),
                size_of::<RegionWithPath>() as i32,
            )
        };
        if n <= 0 {
            break;
        }
        // SAFETY: filled when n > 0.
        let rp = unsafe { rp.assume_init() };
        let ri = rp.info;
        regions += 1;
        let file_backed = rp.path[0] != 0;
        let shared = matches!(ri.share_mode, SM_SHARED | SM_TRUESHARED | SM_SHARED_ALIASED);
        if !file_backed {
            m.private_resident += ri.private_pages_resident as u64 * ps;
            if !shared {
                m.private_swapped += ri.pages_swapped_out as u64 * ps;
            }
        }
        address = ri.address + ri.size;
    }
    (regions > 0).then_some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_machine_and_this_process() {
        let s = system().unwrap();
        assert!(s.mem_total > 0 && s.mem_used > 0 && s.cores > 0);
        assert!(
            [1, 2, 4].contains(&s.pressure),
            "pressure level {}",
            s.pressure
        );

        // 64 MB of touched private memory shows up in the region walk.
        let me = std::process::id() as i32;
        let base = memory(me).unwrap();
        let block = vec![1u8; 64 << 20];
        let grown = memory(me).unwrap();
        std::hint::black_box(&block);
        let delta = (grown.private_resident + grown.private_swapped) as i64
            - (base.private_resident + base.private_swapped) as i64;
        assert!(delta > 48 << 20, "private grew by {delta}");
    }
}
