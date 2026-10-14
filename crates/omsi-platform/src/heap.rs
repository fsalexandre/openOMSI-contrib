//! Memory the process freed, given back to the system where the allocator keeps it.

/// glibc keeps what a thread freed in that thread's arena (one of up to eight a core), and
/// gives it back only from the top of the heap: a server that had loaded a map on every core
/// and seen players come and go held gigabytes it no longer used. `malloc_trim` (glibc 2.8
/// and later) gives back the free pages of every arena, on a thread of its own. Elsewhere
/// it does nothing.
pub fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        let _ = std::thread::Builder::new()
            .name("heap relief".into())
            .spawn(|| {
                let t = std::time::Instant::now();
                // SAFETY: the call takes no pointers; it only gives free pages back
                let freed = unsafe { malloc_trim(0) };
                if omsi_cfg::flags::OMSI_PROFILE.is_set() {
                    log::info!("heap: free pages given back ({}) in {:.0} ms", if freed == 1 { "some" } else { "none" }, t.elapsed().as_secs_f64() * 1000.0);
                }
            });
    }
}
