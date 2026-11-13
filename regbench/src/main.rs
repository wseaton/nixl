use nixl_sys::{Agent, NixlError, NixlRegistration, OptArgs, SystemStorage};
use std::alloc::{AllocError, Allocator, Layout};
use std::error::Error;
use std::fmt;
use std::ptr::{self, NonNull};
use std::time::{Duration, Instant};

const SMALL_PAGE: usize = 4096;
const HUGE_PAGE: usize = 2 << 20;
const MIB: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pages {
    Small,
    TransparentHuge,
}

impl Pages {
    fn granule(self) -> usize {
        match self {
            Pages::Small => SMALL_PAGE,
            Pages::TransparentHuge => HUGE_PAGE,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct MmapAlloc {
    pages: Pages,
}

impl MmapAlloc {
    fn mapped_len(self, layout: Layout) -> Option<usize> {
        let granule = self.pages.granule();
        layout.size().checked_next_multiple_of(granule)
    }
}

#[cfg(target_os = "linux")]
fn advise(addr: *mut libc::c_void, len: usize, pages: Pages) {
    let advice = match pages {
        Pages::Small => libc::MADV_NOHUGEPAGE,
        Pages::TransparentHuge => libc::MADV_HUGEPAGE,
    };
    unsafe { libc::madvise(addr, len, advice) };
}

#[cfg(not(target_os = "linux"))]
fn advise(_addr: *mut libc::c_void, _len: usize, _pages: Pages) {}

unsafe impl Allocator for MmapAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let granule = self.pages.granule();
        if layout.align() > granule {
            return Err(AllocError);
        }
        if layout.size() == 0 {
            let dangling = NonNull::new(ptr::without_provenance_mut::<u8>(layout.align()))
                .ok_or(AllocError)?;
            return Ok(NonNull::slice_from_raw_parts(dangling, 0));
        }
        let len = self.mapped_len(layout).ok_or(AllocError)?;
        let span = len.checked_add(granule).ok_or(AllocError)?;
        let raw = unsafe {
            libc::mmap(
                ptr::null_mut(),
                span,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(AllocError);
        }
        let start = raw as usize;
        let aligned = start.next_multiple_of(granule);
        let head = aligned - start;
        let tail = span - head - len;
        let base = unsafe { raw.cast::<u8>().add(head) };
        unsafe {
            if head > 0 {
                libc::munmap(raw, head);
            }
            if tail > 0 {
                libc::munmap(base.add(len).cast(), tail);
            }
        }
        advise(base.cast(), len, self.pages);
        let base = NonNull::new(base).ok_or(AllocError)?;
        Ok(NonNull::slice_from_raw_parts(base, len))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if let Some(len) = self.mapped_len(layout).filter(|&len| len > 0) {
            unsafe { libc::munmap(ptr.as_ptr().cast(), len) };
        }
    }
}

fn zeroed_mmap_vec(size: usize, alloc: MmapAlloc) -> Vec<u8, MmapAlloc> {
    let mut data = Vec::with_capacity_in(size, alloc);
    unsafe { data.set_len(size) };
    data
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    VecLazy,
    VecTouched,
    Mmap(Pages),
}

impl Mode {
    const ALL: [Mode; 4] = [
        Mode::VecLazy,
        Mode::VecTouched,
        Mode::Mmap(Pages::Small),
        Mode::Mmap(Pages::TransparentHuge),
    ];
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::VecLazy => "vec-lazy",
            Mode::VecTouched => "vec-touched",
            Mode::Mmap(Pages::Small) => "mmap-4k",
            Mode::Mmap(Pages::TransparentHuge) => "mmap-thp",
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    alloc: Duration,
    touch: Duration,
    register: Duration,
    deregister: Duration,
    free: Duration,
    anon_huge_kb: u64,
}

fn anon_huge_kb() -> u64 {
    std::fs::read_to_string("/proc/self/smaps_rollup")
        .ok()
        .and_then(|rollup| {
            rollup
                .lines()
                .find_map(|line| line.strip_prefix("AnonHugePages:"))
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|kb| kb.parse().ok())
        })
        .unwrap_or(0)
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let value = f();
    (value, start.elapsed())
}

fn measure<B>(
    agent: &Agent,
    opt_args: &OptArgs,
    touch: bool,
    make: impl FnOnce() -> Result<SystemStorage<B>, NixlError>,
) -> Result<Sample, NixlError>
where
    B: AsRef<[u8]> + AsMut<[u8]> + fmt::Debug + Send + Sync,
{
    let huge_before = anon_huge_kb();
    let (storage, alloc) = timed(make);
    let mut storage = storage?;
    let touch = if touch {
        timed(|| storage.memset(0)).1
    } else {
        Duration::ZERO
    };
    let (registered, register) = timed(|| storage.register(agent, Some(opt_args)));
    registered?;
    let anon_huge_kb = anon_huge_kb().saturating_sub(huge_before);
    let (buffer, deregister) = timed(|| storage.into_inner());
    let buffer = buffer.map_err(|(_storage, error)| error)?;
    let ((), free) = timed(|| drop(buffer));
    Ok(Sample {
        alloc,
        touch,
        register,
        deregister,
        free,
        anon_huge_kb,
    })
}

fn run(agent: &Agent, opt_args: &OptArgs, mode: Mode, size: usize) -> Result<Sample, NixlError> {
    match mode {
        Mode::VecLazy => measure(agent, opt_args, false, || SystemStorage::new(size)),
        Mode::VecTouched => measure(agent, opt_args, true, || SystemStorage::new(size)),
        Mode::Mmap(pages) => measure(agent, opt_args, true, || {
            let data = zeroed_mmap_vec(size, MmapAlloc { pages });
            Ok(unsafe { SystemStorage::from_buffer(data) })
        }),
    }
}

fn median(samples: &[Sample], field: impl Fn(&Sample) -> Duration) -> f64 {
    let mut values: Vec<Duration> = samples.iter().map(field).collect();
    values.sort();
    values
        .get(values.len() / 2)
        .map_or(0.0, |d| d.as_secs_f64() * 1e3)
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn main() -> Result<(), Box<dyn Error>> {
    let tag = env_or("REGBENCH_TAG", "default");
    let iters: usize = env_or("REGBENCH_ITERS", "10").parse()?;
    let sizes = env_or("REGBENCH_SIZES_MIB", "16,64,256,1024,4096")
        .split(',')
        .map(|mib| mib.trim().parse::<usize>().map(|mib| mib * MIB))
        .collect::<Result<Vec<_>, _>>()?;

    let agent = Agent::new("regbench")?;
    let (_mems, params) = agent.get_plugin_params("UCX")?;
    let backend = agent.create_backend("UCX", &params)?;
    let mut opt_args = OptArgs::new()?;
    opt_args.add_backend(&backend)?;

    println!(
        "tag,mode,size_mib,iter,alloc_us,touch_us,register_us,deregister_us,free_us,anon_huge_kb"
    );
    let mut summary = Vec::new();
    for &size in &sizes {
        for mode in Mode::ALL {
            run(&agent, &opt_args, mode, size)?;
            let mut samples = Vec::with_capacity(iters);
            for iter in 0..iters {
                let s = run(&agent, &opt_args, mode, size)?;
                println!(
                    "{tag},{mode},{},{iter},{},{},{},{},{},{}",
                    size / MIB,
                    s.alloc.as_micros(),
                    s.touch.as_micros(),
                    s.register.as_micros(),
                    s.deregister.as_micros(),
                    s.free.as_micros(),
                    s.anon_huge_kb,
                );
                samples.push(s);
            }
            summary.push((size, mode, samples));
        }
    }

    eprintln!("\n[{tag}] medians over {iters} iterations, ms (first iteration discarded)");
    eprintln!(
        "{:>8} {:<12} {:>9} {:>9} {:>10} {:>11} {:>9} {:>9}",
        "size_mib", "mode", "alloc", "touch", "register", "deregister", "free", "thp_pct"
    );
    for (size, mode, samples) in &summary {
        let huge_kb = samples.iter().map(|s| s.anon_huge_kb).max().unwrap_or(0);
        let thp_pct = 100.0 * (huge_kb * 1024) as f64 / *size as f64;
        eprintln!(
            "{:>8} {:<12} {:>9.3} {:>9.3} {:>10.3} {:>11.3} {:>9.3} {:>9.0}",
            size / MIB,
            mode.to_string(),
            median(samples, |s| s.alloc),
            median(samples, |s| s.touch),
            median(samples, |s| s.register),
            median(samples, |s| s.deregister),
            median(samples, |s| s.free),
            thp_pct,
        );
    }
    Ok(())
}
