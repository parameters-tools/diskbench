use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::env;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

// ==============================================================================
// 1. ASYNC-SIGNAL-SAFE RESOURCE CLEANUP (Handles Ctrl+C / SIGINT / SIGTERM)
// ==============================================================================

static CLEANUP_CSTRING: AtomicPtr<libc::c_char> = AtomicPtr::new(std::ptr::null_mut());

extern "C" fn sig_handler(sig: libc::c_int) {
    let ptr = CLEANUP_CSTRING.swap(std::ptr::null_mut(), Ordering::SeqCst);
    if !ptr.is_null() {
        // POSIX guarantees libc::unlink is async-signal-safe
        unsafe {
            libc::unlink(ptr);
        }
    }
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn register_cleanup_path(path: &Path) {
    if let Ok(c_str) = CString::new(path.to_str().unwrap_or_default()) {
        let raw = c_str.into_raw();
        let prev = CLEANUP_CSTRING.swap(raw, Ordering::SeqCst);
        if !prev.is_null() {
            unsafe {
                drop(CString::from_raw(prev));
            }
        }
    }
    unsafe {
        libc::signal(libc::SIGINT, sig_handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, sig_handler as *const () as libc::sighandler_t);
    }
}

fn unregister_cleanup_path() {
    let prev = CLEANUP_CSTRING.swap(std::ptr::null_mut(), Ordering::SeqCst);
    if !prev.is_null() {
        unsafe {
            drop(CString::from_raw(prev));
        }
    }
}

/// RAII Guard guaranteeing temporary file deletion even on early exit or panic.
struct TempFileGuard {
    path: PathBuf,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        register_cleanup_path(&path);
        Self { path }
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = fs::remove_file(&self.path);
        }
        unregister_cleanup_path();
    }
}

// ==============================================================================
// 2. HARDWARE TELEMETRY & THERMAL MONITORING (Direct Linux Kernel sysfs)
// ==============================================================================

#[derive(Clone, Debug, Default)]
struct DeviceTelemetry {
    device_node: String,
    model: String,
    transport: String,
    pcie_link: String,
    initial_temp_c: Option<f32>,
    temp_sensor_path: Option<PathBuf>,
}

impl DeviceTelemetry {
    fn read_current_temp(&self) -> Option<f32> {
        let path = self.temp_sensor_path.as_ref()?;
        let content = fs::read_to_string(path).ok()?;
        let milli: f32 = content.trim().parse().ok()?;
        Some(milli / 1000.0)
    }
}

#[allow(clippy::unnecessary_cast)]
fn probe_hardware_telemetry(target_path: &Path) -> DeviceTelemetry {
    let mut telem = DeviceTelemetry {
        device_node: "Unknown".to_string(),
        model: "Generic Storage Device".to_string(),
        transport: "Standard I/O".to_string(),
        pcie_link: "N/A".to_string(),
        initial_temp_c: None,
        temp_sensor_path: None,
    };

    let c_path = match CString::new(target_path.to_str().unwrap_or(".")) {
        Ok(c) => c,
        Err(_) => return telem,
    };

    let mut st = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::stat(c_path.as_ptr(), st.as_mut_ptr()) } != 0 {
        return telem;
    }
    let st = unsafe { st.assume_init() };
    let dev = st.st_dev;
    let major = libc::major(dev);
    let minor = libc::minor(dev);

    let sys_dev_link = format!("/sys/dev/block/{}:{}", major, minor);
    let real_sysfs = match fs::canonicalize(&sys_dev_link) {
        Ok(p) => p,
        Err(_) => return telem,
    };

    let real_str = real_sysfs.to_string_lossy();
    if let Some(dev_name) = real_sysfs.file_name() {
        telem.device_node = format!("/dev/{}", dev_name.to_string_lossy());
    }

    if real_str.contains("nvme") {
        telem.transport = "NVMe (PCIe)".to_string();
    } else if real_str.contains("ata") || real_str.contains("scsi") {
        telem.transport = "SATA / SCSI".to_string();
    } else if real_str.contains("usb") {
        telem.transport = "USB Storage".to_string();
    } else if real_str.contains("mmc") {
        telem.transport = "eMMC / SD".to_string();
    }

    // Traverse directory ancestors to locate model, PCIe link, and hwmon temperature
    let mut cur: Option<&Path> = Some(&real_sysfs);
    while let Some(dir) = cur {
        // 1. Model
        if telem.model == "Generic Storage Device" {
            let m_path = dir.join("model");
            if let Ok(m) = fs::read_to_string(m_path) {
                let trimmed = m.trim().to_string();
                if !trimmed.is_empty() {
                    telem.model = trimmed;
                }
            }
        }

        // 2. PCIe Link Speed & Width
        if telem.pcie_link == "N/A" {
            let speed_p = dir.join("current_link_speed");
            let width_p = dir.join("current_link_width");
            if speed_p.exists() && width_p.exists()
                && let (Ok(s), Ok(w)) = (fs::read_to_string(&speed_p), fs::read_to_string(&width_p)) {
                    telem.pcie_link = format!("{} x{}", s.trim(), w.trim());
                }
        }

        // 3. Hardware Thermal Sensor (hwmon)
        if telem.temp_sensor_path.is_none()
            && let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name.starts_with("hwmon") {
                        let t1 = p.join("temp1_input");
                        if t1.exists() {
                            telem.temp_sensor_path = Some(t1);
                            break;
                        }
                    }
                }
            }

        cur = dir.parent();
    }

    telem.initial_temp_c = telem.read_current_temp();
    telem
}

// ==============================================================================
// 3. MEMORY-ALIGNED DMA BUFFER (Required for Linux O_DIRECT)
// ==============================================================================

struct AlignedBuffer {
    ptr: *mut u8,
    layout: Layout,
    len: usize,
}

impl AlignedBuffer {
    fn new(size: usize, align: usize) -> Self {
        assert!(size > 0, "Buffer size must be non-zero");
        assert!(align > 0 && align.is_power_of_two(), "Alignment must be power of 2");

        let layout = Layout::from_size_align(size, align).expect("Invalid layout parameters");
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self { ptr, layout, len: size }
    }

    #[inline(always)]
    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    #[inline(always)]
    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

unsafe impl Send for AlignedBuffer {}
unsafe impl Sync for AlignedBuffer {}

// ==============================================================================
// 4. LATENCY TRACKING & STATISTICAL METRICS (Avg, Min, Max, p95, p99)
// ==============================================================================

#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
struct LatencyStats {
    avg_us: f64,
    min_us: u32,
    max_us: u32,
    p50_us: u32,
    p95_us: u32,
    p99_us: u32,
}

#[derive(Default)]
struct LatencyTracker {
    samples: Vec<u32>, // in microseconds
    sum_micros: u128,
    total_ops: u64,
}

impl LatencyTracker {
    fn new() -> Self {
        Self {
            samples: Vec::with_capacity(16384),
            sum_micros: 0,
            total_ops: 0,
        }
    }

    #[inline(always)]
    fn record(&mut self, micros: u32) {
        self.sum_micros += micros as u128;
        self.total_ops += 1;
        // Cap sample vector to avoid excessive memory on massive tests
        if self.samples.len() < 100_000 {
            self.samples.push(micros);
        }
    }

    fn merge(&mut self, other: LatencyTracker) {
        self.sum_micros += other.sum_micros;
        self.total_ops += other.total_ops;
        if self.samples.len() < 100_000 {
            let take = (100_000 - self.samples.len()).min(other.samples.len());
            self.samples.extend_from_slice(&other.samples[..take]);
        }
    }

    fn calculate(&mut self) -> LatencyStats {
        if self.total_ops == 0 {
            return LatencyStats::default();
        }

        let avg_us = self.sum_micros as f64 / self.total_ops as f64;
        if self.samples.is_empty() {
            return LatencyStats {
                avg_us,
                min_us: 0,
                max_us: 0,
                p50_us: avg_us as u32,
                p95_us: avg_us as u32,
                p99_us: avg_us as u32,
            };
        }

        self.samples.sort_unstable();
        let len = self.samples.len();
        let min_us = self.samples[0];
        let max_us = self.samples[len - 1];
        let p50_us = self.samples[(len * 50) / 100];
        let p95_us = self.samples[((len * 95) / 100).min(len - 1)];
        let p99_us = self.samples[((len * 99) / 100).min(len - 1)];

        LatencyStats {
            avg_us,
            min_us,
            max_us,
            p50_us,
            p95_us,
            p99_us,
        }
    }
}

// Fast 64-bit pseudo-random number generator (Xorshift64)
struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0x853c49e6748fea9b } else { seed },
        }
    }

    #[inline(always)]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

// ==============================================================================
// 5. TERMINAL SPARKLINE & UI HELPERS
// ==============================================================================

const SPARK_CHARS: [char; 8] = [' ', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

struct Sparkline {
    history: Vec<f64>,
    capacity: usize,
}

impl Sparkline {
    fn new(capacity: usize) -> Self {
        Self {
            history: Vec::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, val: f64) {
        if self.history.len() >= self.capacity {
            self.history.remove(0);
        }
        self.history.push(val);
    }

    fn render(&self) -> String {
        if self.history.is_empty() {
            return String::new();
        }
        let min = self.history.iter().fold(f64::INFINITY, |a, &b| a.min(b));
        let max = self.history.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b));
        let span = (max - min).max(0.001);

        let mut s = String::with_capacity(self.history.len());
        for &v in &self.history {
            let normalized = ((v - min) / span).clamp(0.0, 1.0);
            let idx = (normalized * 7.0).round() as usize;
            s.push(SPARK_CHARS[idx.min(7)]);
        }
        s
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const TIB: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0;

    let b = bytes as f64;
    if b >= TIB {
        format!("{:.1}T", b / TIB)
    } else if b >= GIB {
        if b / GIB >= 10.0 {
            format!("{:.0}G", b / GIB)
        } else {
            format!("{:.1}G", b / GIB)
        }
    } else if b >= MIB {
        if b / MIB >= 10.0 {
            format!("{:.0}M", b / MIB)
        } else {
            format!("{:.1}M", b / MIB)
        }
    } else if b >= KIB {
        format!("{:.0}K", b / KIB)
    } else {
        format!("{}B", bytes)
    }
}

fn format_speed(mb_per_sec: f64) -> String {
    if !mb_per_sec.is_finite() || mb_per_sec <= 0.0 {
        return "0.00 MB/s".to_string();
    }
    if mb_per_sec >= 1024.0 {
        format!("{:.2} GB/s", mb_per_sec / 1024.0)
    } else if mb_per_sec >= 10.0 {
        format!("{:.1} MB/s", mb_per_sec)
    } else {
        format!("{:.2} MB/s", mb_per_sec)
    }
}

fn format_latency(us: f64) -> String {
    if us >= 1000.0 {
        format!("{:.2} ms", us / 1000.0)
    } else {
        format!("{:.0} µs", us)
    }
}

fn format_iops(iops: f64) -> String {
    if iops >= 1_000_000.0 {
        format!("{:.2}M", iops / 1_000_000.0)
    } else if iops >= 1_000.0 {
        format!("{:.1}k", iops / 1_000.0)
    } else {
        format!("{:.0}", iops)
    }
}

fn make_bar(pct: u8, width: usize, color_code: &str) -> String {
    let pct = pct.min(100);
    let filled = (pct as usize * width) / 100;
    let unfilled = width.saturating_sub(filled);
    let reset = "\x1b[0m";

    let bar = format!(
        "{}{}{}",
        "█".repeat(filled),
        "\x1b[90m",
        "░".repeat(unfilled)
    );
    format!("{}[{}{}]", color_code, bar, reset)
}

fn is_stdout_tty() -> bool {
    unsafe { libc::isatty(libc::STDOUT_FILENO) != 0 }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct BenchmarkResult {
    test_name: String,
    block_size_str: String,
    transferred_str: String,
    threads: usize,
    elapsed_secs: f64,
    speed_mb_s: f64,
    iops: f64,
    latency: LatencyStats,
}

// ==============================================================================
// 6. DISK CAPACITY & FILE CREATION HELPERS
// ==============================================================================

#[allow(clippy::unnecessary_cast)]
fn get_available_bytes(path: &Path) -> Option<u64> {
    let c_path = CString::new(path.to_str()?).ok()?;
    let mut stat = MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    let block_size = stat.f_frsize as u64;
    Some(stat.f_bavail as u64 * block_size)
}

fn generate_secure_temp_path(target_dir: &Path) -> PathBuf {
    let mut random_bytes = [0u8; 8];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = f.read_exact(&mut random_bytes);
    } else {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0x123456789);
        random_bytes.copy_from_slice(&(nanos as u64).to_le_bytes());
    }
    let token = u64::from_le_bytes(random_bytes);
    let filename = format!(".diskbench_{}_{:016x}.tmp", std::process::id(), token);
    target_dir.join(filename)
}

fn create_and_preallocate_file(
    path: &Path,
    total_bytes: usize,
    use_direct: bool,
) -> io::Result<(File, bool)> {
    let try_direct = use_direct;
    let mut is_direct = false;

    let file = if try_direct {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create_new(true).mode(0o600);
        opts.custom_flags(libc::O_DIRECT);
        match opts.open(path) {
            Ok(f) => {
                is_direct = true;
                f
            }
            Err(_) => {
                let mut opts = OpenOptions::new();
                opts.read(true).write(true).create_new(true).mode(0o600);
                opts.open(path)?
            }
        }
    } else {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create_new(true).mode(0o600);
        opts.open(path)?
    };

    // Preallocate disk space upfront (posix_fallocate or ftruncate)
    let fd = file.as_raw_fd();
    unsafe {
        let ret = libc::posix_fallocate(fd, 0, total_bytes as libc::off_t);
        if ret != 0 {
            let _ = file.set_len(total_bytes as u64);
        }
    }

    Ok((file, is_direct))
}

fn open_thread_file(path: &Path, write: bool, direct: bool) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(write);
    if direct {
        opts.custom_flags(libc::O_DIRECT);
    }
    opts.open(path)
}

fn drop_os_cache(file: &File) {
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

// ==============================================================================
// 7. HIGH-CONCURRENCY MULTI-THREADED BENCHMARK RUNNERS
// ==============================================================================

#[allow(clippy::too_many_arguments)]
fn run_threaded_seq(
    path: &Path,
    total_bytes: usize,
    chunk_size: usize,
    threads: usize,
    is_write: bool,
    use_direct: bool,
    test_title: &str,
    is_tty: bool,
    silent: bool,
) -> io::Result<BenchmarkResult> {
    let threads = threads.max(1);
    let bytes_per_thread = (total_bytes / threads / chunk_size) * chunk_size;
    let actual_total_bytes = bytes_per_thread * threads;

    if !is_write && !use_direct
        && let Ok(f) = File::open(path) {
            drop_os_cache(&f);
        }

    let progress_bytes = Arc::new(AtomicUsize::new(0));
    let stop_signal = Arc::new(AtomicBool::new(false));
    let start_time = Instant::now();

    // Spawner: Thread scope for zero-cost multi-worker parallel execution
    let (combined_latency, elapsed_final) = std::thread::scope(|s| {
        let mut worker_handles = Vec::new();

        for t in 0..threads {
            let thread_progress = Arc::clone(&progress_bytes);
            let thread_stop = Arc::clone(&stop_signal);

            worker_handles.push(s.spawn(move || -> io::Result<LatencyTracker> {
                let file = open_thread_file(path, is_write, use_direct)?;
                let mut buffer = AlignedBuffer::new(chunk_size, 4096);
                if is_write {
                    for (i, b) in buffer.as_mut_slice().iter_mut().enumerate() {
                        *b = ((i + t * 17) % 251) as u8;
                    }
                }

                let mut latency = LatencyTracker::new();
                let thread_offset_start = t * bytes_per_thread;
                let chunks = bytes_per_thread / chunk_size;

                for c in 0..chunks {
                    if thread_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let offset = (thread_offset_start + c * chunk_size) as u64;
                    let op_start = Instant::now();

                    if is_write {
                        file.write_all_at(buffer.as_slice(), offset)?;
                    } else {
                        file.read_exact_at(buffer.as_mut_slice(), offset)?;
                    }

                    let op_micros = op_start.elapsed().as_micros() as u32;
                    latency.record(op_micros);
                    thread_progress.fetch_add(chunk_size, Ordering::Relaxed);
                }

                if is_write {
                    file.sync_all()?;
                }

                Ok(latency)
            }));
        }

        // Live progress monitoring on main thread
        let mut last_update = Instant::now();
        let mut sparkline = Sparkline::new(10);
        let bar_color = if is_write { "\x1b[36m" } else { "\x1b[32m" };

        loop {
            let done = progress_bytes.load(Ordering::Relaxed);
            let is_done = done >= actual_total_bytes;

            if !silent && is_tty && (last_update.elapsed().as_millis() >= 40 || is_done) {
                let pct = (((done as f64) / (actual_total_bytes as f64)) * 100.0) as u8;
                let elapsed = start_time.elapsed().as_secs_f64().max(0.0001);
                let current_mb = (done as f64) / (1024.0 * 1024.0);
                let current_speed = current_mb / elapsed;
                sparkline.push(current_speed);

                let bar = make_bar(pct, 16, bar_color);
                print!(
                    "\r\x1b[2K  ▶ \x1b[1m{:<18}\x1b[0m {} {:>3}%  {:>11}  \x1b[33m{}\x1b[0m  ({:.1}s)",
                    test_title, bar, pct, format_speed(current_speed), sparkline.render(), elapsed
                );
                let _ = io::stdout().flush();
                last_update = Instant::now();
            }

            if is_done {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let mut total_lat = LatencyTracker::new();
        for h in worker_handles {
            if let Ok(Ok(lat)) = h.join() {
                total_lat.merge(lat);
            }
        }

        let final_elapsed = start_time.elapsed().as_secs_f64().max(0.0001);
        (total_lat, final_elapsed)
    });

    let mut lat_stats = combined_latency;
    let stats = lat_stats.calculate();
    let speed_mb_s = (actual_total_bytes as f64 / (1024.0 * 1024.0)) / elapsed_final;
    let iops = (actual_total_bytes / chunk_size) as f64 / elapsed_final;

    if !silent {
        let bar_color = if is_write { "\x1b[36m" } else { "\x1b[32m" };
        let bar = make_bar(100, 16, bar_color);
        if is_tty {
            println!(
                "\r\x1b[2K  ✔ \x1b[1m{:<18}\x1b[0m {} 100%  {:>11}  [p95: {:>7}]  ({:.2}s)",
                test_title, bar, format_speed(speed_mb_s), format_latency(stats.p95_us as f64), elapsed_final
            );
        } else {
            println!(
                "  ✔ {:<18} {} 100%  {:>11}  [p95: {:>7}]  ({:.2}s)",
                test_title, bar, format_speed(speed_mb_s), format_latency(stats.p95_us as f64), elapsed_final
            );
        }
    }

    Ok(BenchmarkResult {
        test_name: test_title.to_string(),
        block_size_str: format_bytes(chunk_size as u64),
        transferred_str: format_bytes(actual_total_bytes as u64),
        threads,
        elapsed_secs: elapsed_final,
        speed_mb_s,
        iops,
        latency: stats,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_threaded_rnd_4k(
    path: &Path,
    file_size: usize,
    test_bytes: usize,
    threads: usize,
    is_write: bool,
    use_direct: bool,
    test_title: &str,
    is_tty: bool,
    silent: bool,
) -> io::Result<BenchmarkResult> {
    let threads = threads.max(1);
    let block_size = 4096;
    let num_blocks = file_size / block_size;
    if num_blocks == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "File size too small for 4K test"));
    }

    let ops_per_thread = (test_bytes / block_size) / threads;
    let actual_total_ops = ops_per_thread * threads;
    let actual_total_bytes = actual_total_ops * block_size;

    if !is_write && !use_direct
        && let Ok(f) = File::open(path) {
            drop_os_cache(&f);
        }

    let progress_ops = Arc::new(AtomicUsize::new(0));
    let stop_signal = Arc::new(AtomicBool::new(false));
    let start_time = Instant::now();

    let (combined_latency, elapsed_final) = std::thread::scope(|s| {
        let mut worker_handles = Vec::new();

        for t in 0..threads {
            let thread_progress = Arc::clone(&progress_ops);
            let thread_stop = Arc::clone(&stop_signal);

            worker_handles.push(s.spawn(move || -> io::Result<LatencyTracker> {
                let file = open_thread_file(path, is_write, use_direct)?;
                let mut buffer = AlignedBuffer::new(block_size, 4096);
                if is_write {
                    for (i, b) in buffer.as_mut_slice().iter_mut().enumerate() {
                        *b = ((i + t * 31) % 251) as u8;
                    }
                }

                let mut rng = SimpleRng::new(0xDEADBEEFCAFE0000u64 + (t as u64 * 0x9e3779b97f4a7c15));
                let mut latency = LatencyTracker::new();

                for i in 0..ops_per_thread {
                    if thread_stop.load(Ordering::Relaxed) {
                        break;
                    }

                    let block_idx = (rng.next_u64() as usize) % num_blocks;
                    let offset = (block_idx * block_size) as u64;

                    let op_start = Instant::now();
                    if is_write {
                        file.write_all_at(buffer.as_slice(), offset)?;
                    } else {
                        file.read_exact_at(buffer.as_mut_slice(), offset)?;
                    }
                    let op_micros = op_start.elapsed().as_micros() as u32;
                    latency.record(op_micros);

                    if i & 63 == 0 || i + 1 == ops_per_thread {
                        thread_progress.fetch_add(64.min(ops_per_thread - i), Ordering::Relaxed);
                    }
                }

                if is_write {
                    file.sync_all()?;
                }

                Ok(latency)
            }));
        }

        // Live progress monitoring on main thread
        let mut last_update = Instant::now();
        let mut sparkline = Sparkline::new(10);
        let bar_color = if is_write { "\x1b[33m" } else { "\x1b[35m" };

        loop {
            let done = progress_ops.load(Ordering::Relaxed);
            let is_done = done >= actual_total_ops;

            if !silent && is_tty && (last_update.elapsed().as_millis() >= 40 || is_done) {
                let pct = (((done as f64) / (actual_total_ops as f64)) * 100.0) as u8;
                let elapsed = start_time.elapsed().as_secs_f64().max(0.0001);
                let current_mb = (done * block_size) as f64 / (1024.0 * 1024.0);
                let current_speed = current_mb / elapsed;
                sparkline.push(current_speed);

                let bar = make_bar(pct, 16, bar_color);
                print!(
                    "\r\x1b[2K  ▶ \x1b[1m{:<18}\x1b[0m {} {:>3}%  {:>11}  \x1b[33m{}\x1b[0m  ({:.1}s)",
                    test_title, bar, pct, format_speed(current_speed), sparkline.render(), elapsed
                );
                let _ = io::stdout().flush();
                last_update = Instant::now();
            }

            if is_done {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let mut total_lat = LatencyTracker::new();
        for h in worker_handles {
            if let Ok(Ok(lat)) = h.join() {
                total_lat.merge(lat);
            }
        }

        let final_elapsed = start_time.elapsed().as_secs_f64().max(0.0001);
        (total_lat, final_elapsed)
    });

    let mut lat_stats = combined_latency;
    let stats = lat_stats.calculate();
    let speed_mb_s = (actual_total_bytes as f64 / (1024.0 * 1024.0)) / elapsed_final;
    let iops = actual_total_ops as f64 / elapsed_final;

    if !silent {
        let bar_color = if is_write { "\x1b[33m" } else { "\x1b[35m" };
        let bar = make_bar(100, 16, bar_color);
        if is_tty {
            println!(
                "\r\x1b[2K  ✔ \x1b[1m{:<18}\x1b[0m {} 100%  {:>11}  [p95: {:>7}]  ({:.2}s)",
                test_title, bar, format_speed(speed_mb_s), format_latency(stats.p95_us as f64), elapsed_final
            );
        } else {
            println!(
                "  ✔ {:<18} {} 100%  {:>11}  [p95: {:>7}]  ({:.2}s)",
                test_title, bar, format_speed(speed_mb_s), format_latency(stats.p95_us as f64), elapsed_final
            );
        }
    }

    Ok(BenchmarkResult {
        test_name: test_title.to_string(),
        block_size_str: format_bytes(block_size as u64),
        transferred_str: format_bytes(actual_total_bytes as u64),
        threads,
        elapsed_secs: elapsed_final,
        speed_mb_s,
        iops,
        latency: stats,
    })
}

// ==============================================================================
// 8. JSON EXPORT & COMPARATIVE BENCHMARK PARSER
// ==============================================================================

fn serialize_to_json(
    telem: &DeviceTelemetry,
    target_path: &str,
    total_bytes: usize,
    results: &[BenchmarkResult],
) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  \"tool\": \"diskbench\",\n");
    out.push_str("  \"version\": \"0.2.0\",\n");
    out.push_str("  \"target\": {\n");
    out.push_str(&format!("    \"path\": \"{}\",\n", target_path));
    out.push_str(&format!("    \"device\": \"{}\",\n", telem.device_node));
    out.push_str(&format!("    \"model\": \"{}\",\n", telem.model));
    out.push_str(&format!("    \"transport\": \"{}\",\n", telem.transport));
    out.push_str(&format!("    \"pcie_link\": \"{}\",\n", telem.pcie_link));
    if let Some(t) = telem.read_current_temp() {
        out.push_str(&format!("    \"temperature_celsius\": {:.1},\n", t));
    }
    out.push_str(&format!("    \"file_size_bytes\": {}\n", total_bytes));
    out.push_str("  },\n");
    out.push_str("  \"results\": [\n");

    for (i, r) in results.iter().enumerate() {
        out.push_str("    {\n");
        out.push_str(&format!("      \"test_name\": \"{}\",\n", r.test_name));
        out.push_str(&format!("      \"block_size\": \"{}\",\n", r.block_size_str));
        out.push_str(&format!("      \"transferred\": \"{}\",\n", r.transferred_str));
        out.push_str(&format!("      \"threads\": {},\n", r.threads));
        out.push_str(&format!("      \"speed_mb_s\": {:.2},\n", r.speed_mb_s));
        out.push_str(&format!("      \"iops\": {:.0},\n", r.iops));
        out.push_str(&format!("      \"avg_latency_us\": {:.2},\n", r.latency.avg_us));
        out.push_str(&format!("      \"p95_latency_us\": {},\n", r.latency.p95_us));
        out.push_str(&format!("      \"p99_latency_us\": {}\n", r.latency.p99_us));
        if i + 1 == results.len() {
            out.push_str("    }\n");
        } else {
            out.push_str("    },\n");
        }
    }

    out.push_str("  ]\n");
    out.push_str("}\n");
    out
}

/// Robust parser to extract (test_name, speed_mb_s) pairs from saved baseline JSON.
fn parse_baseline_json(content: &str) -> Vec<(String, f64)> {
    let mut pairs = Vec::new();
    let mut pos = 0;

    while let Some(idx) = content[pos..].find("\"test_name\"") {
        let name_key = pos + idx + "\"test_name\"".len();
        if let Some(colon) = content[name_key..].find(':') {
            let after_colon = name_key + colon + 1;
            if let Some(q1) = content[after_colon..].find('"') {
                let val_start = after_colon + q1 + 1;
                if let Some(q2) = content[val_start..].find('"') {
                    let name = &content[val_start..val_start + q2];
                    let search_from = val_start + q2 + 1;

                    if let Some(s_idx) = content[search_from..].find("\"speed_mb_s\"") {
                        let speed_key = search_from + s_idx + "\"speed_mb_s\"".len();
                        if let Some(s_colon) = content[speed_key..].find(':') {
                            let speed_val_start = speed_key + s_colon + 1;
                            let end_val = content[speed_val_start..]
                                .find([',', '}', '\n', '\r'])
                                .unwrap_or(content.len() - speed_val_start);
                            let speed_str = content[speed_val_start..speed_val_start + end_val].trim();
                            if let Ok(speed) = speed_str.parse::<f64>() {
                                pairs.push((name.to_string(), speed));
                            }
                        }
                    }
                    pos = search_from;
                    continue;
                }
            }
        }
        pos += idx + "\"test_name\"".len();
    }

    pairs
}

// ==============================================================================
// 9. ARGUMENT PARSER
// ==============================================================================

fn parse_size(arg: &str) -> Option<usize> {
    let s = arg.trim();
    if s.is_empty() {
        return None;
    }
    let (num_part, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(s.len()),
    );
    let val: f64 = num_part.parse().ok()?;
    if val <= 0.0 || !val.is_finite() {
        return None;
    }

    let unit = unit.trim().to_uppercase();
    let bytes = match unit.as_str() {
        "T" | "TB" | "TIB" => val * 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "G" | "GB" | "GIB" => val * 1024.0 * 1024.0 * 1024.0,
        "M" | "MB" | "MIB" => val * 1024.0 * 1024.0,
        "K" | "KB" | "KIB" => val * 1024.0,
        "" | "B" => val,
        _ => return None,
    };

    if bytes > (usize::MAX as f64) {
        return None;
    }
    Some(bytes as usize)
}

fn print_help() {
    println!();
    println!("⚡ \x1b[1mDISK SPEED BENCHMARK (Direct Kernel I/O)\x1b[0m");
    println!("--------------------------------------------------------------------------------------------------");
    println!("Usage: diskbench [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -p, --path <DIR>          Target directory or mount to benchmark (default: current dir)");
    println!("  -s, --size <SIZE>         Benchmark file size (e.g. 512M, 1G, 2G; default: 512M)");
    println!("  -t, --threads <N>         Worker thread concurrency for parallel queues (default: auto)");
    println!("  -a, --all                 Run complete CrystalDiskMark profile suite (Q8T1, Q1T1, Q32T16)");
    println!("  -r, --runs <N>            Multi-pass benchmarking with average and standard deviation (default: 1)");
    println!("      --json                Emit machine-readable JSON to stdout (disables UI)");
    println!("      --save <FILE>         Save benchmark results to a JSON baseline file");
    println!("      --compare <FILE>      Compare current performance against a previous baseline JSON");
    println!("      --no-direct           Disable O_DIRECT (use OS page cache + fsync)");
    println!("  -h, --help                Show this help screen");
    println!("--------------------------------------------------------------------------------------------------");
    println!();
}

// ==============================================================================
// 10. MAIN ENTRYPOINT
// ==============================================================================

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut target_dir = PathBuf::from(".");
    let mut requested_bytes = 512 * 1024 * 1024; // 512 MB default
    let mut custom_threads: Option<usize> = None;
    let mut run_all_profiles = false;
    let mut num_runs = 1;
    let mut use_direct = true;
    let mut output_json = false;
    let mut save_path: Option<PathBuf> = None;
    let mut compare_path: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_help();
                return;
            }
            "-a" | "--all" => {
                run_all_profiles = true;
            }
            "--json" => {
                output_json = true;
            }
            "--no-direct" => {
                use_direct = false;
            }
            "-p" | "--path" => {
                if i + 1 < args.len() {
                    target_dir = PathBuf::from(&args[i + 1]);
                    i += 1;
                } else {
                    eprintln!("❌ Missing argument for --path");
                    std::process::exit(1);
                }
            }
            "-s" | "--size" => {
                if i + 1 < args.len() {
                    if let Some(sz) = parse_size(&args[i + 1]) {
                        requested_bytes = sz;
                    } else {
                        eprintln!("❌ Invalid size format: '{}'. Examples: 512M, 1G, 2G", args[i + 1]);
                        std::process::exit(1);
                    }
                    i += 1;
                } else {
                    eprintln!("❌ Missing argument for --size");
                    std::process::exit(1);
                }
            }
            "-t" | "--threads" => {
                if i + 1 < args.len() {
                    if let Ok(th) = args[i + 1].parse::<usize>() {
                        custom_threads = Some(th.clamp(1, 64));
                    }
                    i += 1;
                }
            }
            "-r" | "--runs" => {
                if i + 1 < args.len() {
                    if let Ok(r) = args[i + 1].parse::<usize>() {
                        num_runs = r.clamp(1, 10);
                    }
                    i += 1;
                }
            }
            "--save" => {
                if i + 1 < args.len() {
                    save_path = Some(PathBuf::from(&args[i + 1]));
                    i += 1;
                }
            }
            "--compare" => {
                if i + 1 < args.len() {
                    compare_path = Some(PathBuf::from(&args[i + 1]));
                    i += 1;
                }
            }
            other => {
                eprintln!("❌ Unknown option: '{}'. Run 'diskbench --help' for usage.", other);
                std::process::exit(1);
            }
        }
        i += 1;
    }

    // 1. Validate target directory exists and is a directory
    if !target_dir.exists() {
        eprintln!("❌ Target directory does not exist: {}", target_dir.display());
        std::process::exit(1);
    }
    if !target_dir.is_dir() {
        eprintln!("❌ Target path is not a directory: {}", target_dir.display());
        std::process::exit(1);
    }

    // 2. Canonicalize path & probe hardware telemetry
    let display_path = target_dir.canonicalize().unwrap_or_else(|_| target_dir.clone());
    let path_str = display_path.to_string_lossy();
    let telem = probe_hardware_telemetry(&display_path);

    // 3. Size alignment: 1MB chunk alignment, minimum 4MB
    let chunk_size = 1024 * 1024;
    let min_bytes = 4 * 1024 * 1024;
    let total_bytes = requested_bytes.max(min_bytes).div_ceil(chunk_size) * chunk_size;

    // 4. Pre-flight disk space capacity check (+ 50MB safety buffer)
    if let Some(avail_bytes) = get_available_bytes(&target_dir) {
        let margin_bytes = 50 * 1024 * 1024;
        if avail_bytes < (total_bytes as u64) + margin_bytes {
            eprintln!("❌ Insufficient disk space on target directory:");
            eprintln!("   Target path:      {}", path_str);
            eprintln!("   Requested size:   {}", format_bytes(total_bytes as u64));
            eprintln!("   Available space:  {}", format_bytes(avail_bytes));
            eprintln!("   Safety margin:    50M");
            eprintln!("   Please specify a smaller size using 'diskbench --size <SIZE>'");
            std::process::exit(1);
        }
    }

    // 5. Generate secure non-predictable temporary file path & RAII cleanup guard
    let test_file_path = generate_secure_temp_path(&target_dir);
    let _guard = TempFileGuard::new(test_file_path.clone());

    // 6. Preallocate test file
    let (prealloc_file, is_direct_effective) =
        match create_and_preallocate_file(&test_file_path, total_bytes, use_direct) {
            Ok(res) => res,
            Err(e) => {
                eprintln!("❌ Failed to initialize benchmark test file: {}", e);
                std::process::exit(1);
            }
        };
    drop(prealloc_file); // Close initial descriptor so workers can open per-thread fds

    let is_tty = is_stdout_tty() && !output_json;
    let width = 98;
    let divider = "-".repeat(width);

    // Render interactive banner if not JSON
    if !output_json {
        println!();
        println!("⚡ \x1b[1mDISK PERFORMANCE BENCHMARK (Direct Kernel I/O)\x1b[0m");
        println!("{}", divider);
        println!(
            "\x1b[1mTARGET:\x1b[0m {:<22} | \x1b[1mMODEL:\x1b[0m {:<36}",
            if path_str.len() > 22 { &path_str[..22] } else { &path_str },
            if telem.model.len() > 36 { &telem.model[..36] } else { &telem.model }
        );
        let temp_str = telem.initial_temp_c.map(|t| format!("{:.1}°C", t)).unwrap_or_else(|| "N/A".to_string());
        println!(
            "\x1b[1mTRANS:\x1b[0m  {:<22} | \x1b[1mPCIE:\x1b[0m  {:<16} | \x1b[1mTEMP:\x1b[0m {:<6} | \x1b[1mSIZE:\x1b[0m {:<6}",
            telem.transport, telem.pcie_link, temp_str, format_bytes(total_bytes as u64)
        );
        println!(
            "\x1b[1mDIRECT I/O:\x1b[0m {}",
            if is_direct_effective {
                "\x1b[32mENABLED (O_DIRECT)\x1b[0m"
            } else if use_direct {
                "\x1b[33mFALLBACK (fsync)\x1b[0m"
            } else {
                "\x1b[33mDISABLED\x1b[0m"
            }
        );

        if use_direct && !is_direct_effective {
            println!("\x1b[33m⚠️  Notice: Filesystem rejected O_DIRECT. Using buffered I/O with page cache dropping.\x1b[0m");
        }
        println!("{}", divider);
        println!("Running benchmark tests...\n");
    }

    // 7. Run Test Suite (with Multi-Pass Averaging if --runs > 1)
    let mut all_pass_results: Vec<Vec<BenchmarkResult>> = Vec::new();

    for run_idx in 0..num_runs {
        if !output_json && num_runs > 1 {
            println!("\x1b[1m--- Run {} of {} ---\x1b[0m", run_idx + 1, num_runs);
        }

        let mut current_run: Vec<BenchmarkResult> = Vec::new();

        if run_all_profiles {
            // Full CrystalDiskMark Profile Suite:
            // 1. SEQ1M Q8T1 Write & Read
            if let Ok(res) = run_threaded_seq(&test_file_path, total_bytes, chunk_size, 8, true, is_direct_effective, "SEQ1M Q8T1 Write", is_tty, output_json) {
                current_run.push(res);
            }
            if let Ok(res) = run_threaded_seq(&test_file_path, total_bytes, chunk_size, 8, false, is_direct_effective, "SEQ1M Q8T1 Read", is_tty, output_json) {
                current_run.push(res);
            }

            // 2. SEQ1M Q1T1 Write & Read (Single Stream)
            if let Ok(res) = run_threaded_seq(&test_file_path, total_bytes, chunk_size, 1, true, is_direct_effective, "SEQ1M Q1T1 Write", is_tty, output_json) {
                current_run.push(res);
            }
            if let Ok(res) = run_threaded_seq(&test_file_path, total_bytes, chunk_size, 1, false, is_direct_effective, "SEQ1M Q1T1 Read", is_tty, output_json) {
                current_run.push(res);
            }

            // 3. RND4K Q32T16 Write & Read (Queue Saturation / Peak IOPS)
            let rnd_bytes = (total_bytes / 4).clamp(16 * 1024 * 1024, 64 * 1024 * 1024);
            if let Ok(res) = run_threaded_rnd_4k(&test_file_path, total_bytes, rnd_bytes, 16, true, is_direct_effective, "RND4K Q32T16 Write", is_tty, output_json) {
                current_run.push(res);
            }
            if let Ok(res) = run_threaded_rnd_4k(&test_file_path, total_bytes, rnd_bytes, 16, false, is_direct_effective, "RND4K Q32T16 Read", is_tty, output_json) {
                current_run.push(res);
            }

            // 4. RND4K Q1T1 Write & Read (Single-Queue Latency / OS Responsiveness)
            let rnd_q1_bytes = (rnd_bytes / 2).max(8 * 1024 * 1024);
            if let Ok(res) = run_threaded_rnd_4k(&test_file_path, total_bytes, rnd_q1_bytes, 1, true, is_direct_effective, "RND4K Q1T1 Write", is_tty, output_json) {
                current_run.push(res);
            }
            if let Ok(res) = run_threaded_rnd_4k(&test_file_path, total_bytes, rnd_q1_bytes, 1, false, is_direct_effective, "RND4K Q1T1 Read", is_tty, output_json) {
                current_run.push(res);
            }
        } else {
            // Standard Mode
            let threads = custom_threads.unwrap_or(1);
            let seq_title_w = if threads > 1 { format!("Seq Write (T{})", threads) } else { "Seq Write".to_string() };
            let seq_title_r = if threads > 1 { format!("Seq Read (T{})", threads) } else { "Seq Read".to_string() };

            if let Ok(res) = run_threaded_seq(&test_file_path, total_bytes, chunk_size, threads, true, is_direct_effective, &seq_title_w, is_tty, output_json) {
                current_run.push(res);
            }
            if let Ok(res) = run_threaded_seq(&test_file_path, total_bytes, chunk_size, threads, false, is_direct_effective, &seq_title_r, is_tty, output_json) {
                current_run.push(res);
            }

            let rnd_bytes = (total_bytes / 8).clamp(8 * 1024 * 1024, 32 * 1024 * 1024);
            let rnd_title_w = if threads > 1 { format!("Rnd 4K Write (T{})", threads) } else { "Rnd 4K Write".to_string() };
            let rnd_title_r = if threads > 1 { format!("Rnd 4K Read (T{})", threads) } else { "Rnd 4K Read".to_string() };

            if let Ok(res) = run_threaded_rnd_4k(&test_file_path, total_bytes, rnd_bytes, threads, true, is_direct_effective, &rnd_title_w, is_tty, output_json) {
                current_run.push(res);
            }
            if let Ok(res) = run_threaded_rnd_4k(&test_file_path, total_bytes, rnd_bytes, threads, false, is_direct_effective, &rnd_title_r, is_tty, output_json) {
                current_run.push(res);
            }
        }

        all_pass_results.push(current_run);
    }

    // Average the results across runs
    let first_run = &all_pass_results[0];
    let mut final_results: Vec<BenchmarkResult> = Vec::new();

    for (test_idx, template) in first_run.iter().enumerate() {
        let mut speed_sum = 0.0;
        let mut iops_sum = 0.0;
        let mut p95_sum = 0.0;

        for run in &all_pass_results {
            if let Some(r) = run.get(test_idx) {
                speed_sum += r.speed_mb_s;
                iops_sum += r.iops;
                p95_sum += r.latency.p95_us as f64;
            }
        }

        let n = all_pass_results.len() as f64;
        let mut averaged = template.clone();
        averaged.speed_mb_s = speed_sum / n;
        averaged.iops = iops_sum / n;
        averaged.latency.p95_us = (p95_sum / n).round() as u32;
        final_results.push(averaged);
    }

    // Thermal Check at end of run
    let final_temp = telem.read_current_temp();

    // 8. Output handling (JSON or Terminal Table)
    let json_data = serialize_to_json(&telem, &path_str, total_bytes, &final_results);

    if let Some(ref s_path) = save_path {
        if let Err(e) = fs::write(s_path, &json_data) {
            eprintln!("❌ Failed to save benchmark to {}: {}", s_path.display(), e);
        } else if !output_json {
            println!("💾 Baseline successfully saved to: {}", s_path.display());
        }
    }

    if output_json {
        print!("{}", json_data);
        return;
    }

    // Check if comparison baseline was provided
    let mut baseline_map: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    if let Some(ref c_path) = compare_path {
        if let Ok(content) = fs::read_to_string(c_path) {
            for (name, speed) in parse_baseline_json(&content) {
                baseline_map.insert(name, speed);
            }
        } else {
            eprintln!("⚠️  Could not read baseline comparison file: {}", c_path.display());
        }
    }

    // --- RENDER ADVANCED SUMMARY TABLE ---
    println!();
    println!("📊 \x1b[1mBENCHMARK SUMMARY RESULTS\x1b[0m");
    println!("{}", divider);

    if !baseline_map.is_empty() {
        // Comparative Table
        println!(
            "\x1b[1m{:<20} {:>12} {:>14} {:>10}   {:<20} {:>8}\x1b[0m",
            "TEST PROFILE", "BASELINE", "CURRENT SPEED", "DIFF %", "PERFORMANCE BAR", "LAT (p95)"
        );
        println!("{}", divider);

        let max_speed = final_results.iter().map(|r| r.speed_mb_s).fold(1.0, f64::max);

        for r in &final_results {
            let base_speed = baseline_map.get(&r.test_name).copied().unwrap_or(0.0);
            let diff_str = if base_speed > 0.0 {
                let diff = ((r.speed_mb_s - base_speed) / base_speed) * 100.0;
                if diff >= 0.0 {
                    format!("\x1b[32m+{:.1}% ▲\x1b[0m", diff)
                } else {
                    format!("\x1b[31m{:.1}% ▼\x1b[0m", diff)
                }
            } else {
                "---".to_string()
            };

            let score_pct = ((r.speed_mb_s / max_speed) * 100.0).clamp(5.0, 100.0) as u8;
            let bar = make_bar(score_pct, 14, "\x1b[32m");

            println!(
                "{:<20} {:>12} {:>14} {:>16}   {} {:>8}",
                r.test_name,
                if base_speed > 0.0 { format_speed(base_speed) } else { "N/A".to_string() },
                format_speed(r.speed_mb_s),
                diff_str,
                bar,
                format_latency(r.latency.p95_us as f64)
            );
        }
    } else {
        // Standard Advanced Table
        println!(
            "\x1b[1m{:<20} {:>12} {:>10} {:>10}   {:<20} {:>6} {:>8}\x1b[0m",
            "TEST PROFILE", "SPEED", "IOPS", "LAT (p95)", "PERFORMANCE BAR", "RATING", "THREADS"
        );
        println!("{}", divider);

        let max_speed = final_results.iter().map(|r| r.speed_mb_s).fold(1.0, f64::max);

        for r in &final_results {
            let score_pct = ((r.speed_mb_s / max_speed) * 100.0).clamp(5.0, 100.0) as u8;
            let bar = make_bar(score_pct, 14, "\x1b[32m");
            let rating = if r.speed_mb_s >= 2000.0 {
                "\x1b[35mULTRA\x1b[0m"
            } else if r.speed_mb_s >= 500.0 {
                "\x1b[32mFAST\x1b[0m"
            } else if r.speed_mb_s >= 100.0 {
                "\x1b[36mGOOD\x1b[0m"
            } else {
                "\x1b[33mSLOW\x1b[0m"
            };

            println!(
                "{:<20} {:>12} {:>10} {:>10}   {} {:>6} {:>8}",
                r.test_name,
                format_speed(r.speed_mb_s),
                format_iops(r.iops),
                format_latency(r.latency.p95_us as f64),
                bar,
                rating,
                r.threads
            );
        }
    }

    println!("{}", divider);

    // Thermal alert check
    if let (Some(init_t), Some(end_t)) = (telem.initial_temp_c, final_temp) {
        if end_t >= 70.0 {
            println!(
                "\x1b[31m🔥 WARNING: High drive temperature detected: {:.1}°C (started at {:.1}°C). Thermal throttling may have occurred!\x1b[0m",
                end_t, init_t
            );
        } else {
            println!(
                "\x1b[90mThermal status: {:.1}°C (initial: {:.1}°C, Δ: {:+.1}°C)\x1b[0m",
                end_t, init_t, end_t - init_t
            );
        }
    }

    if !run_all_profiles && compare_path.is_none() {
        println!(
            "\x1b[90mTip: Run with --all to test the full CrystalDiskMark suite (Q8T1, Q1T1, Q32T16).\x1b[0m\n"
        );
    } else {
        println!();
    }
}

// ==============================================================================
// 11. UNIT TESTS
// ==============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_size_valid() {
        assert_eq!(parse_size("512M"), Some(512 * 1024 * 1024));
        assert_eq!(parse_size("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("2.5G"), Some((2.5 * 1024.0 * 1024.0 * 1024.0) as usize));
        assert_eq!(parse_size("64KB"), Some(64 * 1024));
        assert_eq!(parse_size("1048576B"), Some(1048576));
    }

    #[test]
    fn test_parse_size_invalid() {
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("-500M"), None);
        assert_eq!(parse_size("0M"), None);
        assert_eq!(parse_size("invalid"), None);
        assert_eq!(parse_size("100000000000000000000TB"), None);
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500B");
        assert_eq!(format_bytes(1024), "1K");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10M");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0G");
        assert_eq!(format_bytes(10 * 1024 * 1024 * 1024), "10G");
    }

    #[test]
    fn test_format_speed() {
        assert_eq!(format_speed(0.0), "0.00 MB/s");
        assert_eq!(format_speed(5.25), "5.25 MB/s");
        assert_eq!(format_speed(250.0), "250.0 MB/s");
        assert_eq!(format_speed(2048.0), "2.00 GB/s");
    }

    #[test]
    fn test_format_latency() {
        assert_eq!(format_latency(45.0), "45 µs");
        assert_eq!(format_latency(1500.0), "1.50 ms");
    }

    #[test]
    fn test_aligned_buffer() {
        let mut buf = AlignedBuffer::new(4096, 4096);
        assert_eq!(buf.ptr as usize % 4096, 0);
        assert_eq!(buf.as_slice().len(), 4096);
        buf.as_mut_slice()[0] = 42;
        assert_eq!(buf.as_slice()[0], 42);
    }

    #[test]
    fn test_rng_determinism() {
        let mut rng1 = SimpleRng::new(12345);
        let mut rng2 = SimpleRng::new(12345);
        assert_eq!(rng1.next_u64(), rng2.next_u64());
        assert_eq!(rng1.next_u64(), rng2.next_u64());
    }

    #[test]
    fn test_sparkline_render() {
        let mut sp = Sparkline::new(5);
        sp.push(10.0);
        sp.push(20.0);
        sp.push(30.0);
        sp.push(40.0);
        sp.push(50.0);
        let s = sp.render();
        assert_eq!(s.chars().count(), 5);
    }

    #[test]
    fn test_latency_tracker_percentiles() {
        let mut tracker = LatencyTracker::new();
        for i in 1..=100 {
            tracker.record(i);
        }
        let stats = tracker.calculate();
        assert_eq!(stats.min_us, 1);
        assert_eq!(stats.max_us, 100);
        assert_eq!(stats.p50_us, 51);
        assert_eq!(stats.p95_us, 96);
        assert_eq!(stats.p99_us, 100);
    }

    #[test]
    fn test_baseline_json_parser() {
        let json = r#"
        {
            "results": [
                { "test_name": "Seq Write", "speed_mb_s": 2500.5 },
                { "test_name": "Seq Read", "speed_mb_s": 2200.0 }
            ]
        }
        "#;
        let pairs = parse_baseline_json(json);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "Seq Write");
        assert_eq!(pairs[0].1, 2500.5);
        assert_eq!(pairs[1].0, "Seq Read");
        assert_eq!(pairs[1].1, 2200.0);
    }
}
