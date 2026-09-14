//! Release session-owned allocations first, then ask the platform's system allocator to purge.
//! This is best-effort, not an RSS guarantee (nor a purge of a custom Rust global allocator).

#[cfg(target_os = "macos")]
pub(crate) const ENFORCEMENT: &str = "session/cache dropped; macOS malloc_zone_pressure_relief, best effort";
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) const ENFORCEMENT: &str = "session/cache dropped; glibc malloc_trim, best effort";
#[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
pub(crate) const ENFORCEMENT: &str = "session/cache dropped; no system allocator purge API on this platform";

#[allow(unsafe_code)]
pub(crate) fn allocator_purge() {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: Darwin's malloc API explicitly accepts NULL for all zones and zero for
        // maximal pressure relief. No user allocation or pointer is dereferenced here.
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: malloc_trim has no pointer arguments; zero keeps no extra trailing pages.
        unsafe {
            libc::malloc_trim(0);
        }
    }
}
