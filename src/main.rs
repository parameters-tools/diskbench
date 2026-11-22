use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::env;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Instant;

// ==============================================================================
// 1. ASYNC-SIGNAL-SAFE RESOURCE CLEANUP (Handles Ctrl+C / SIGINT / SIGTERM)
// ==============================================================================

static CLEANUP_CSTRING: AtomicPtr<libc::c_char> = AtomicPtr::new(std::ptr::null_mut());

extern "C" fn sig_handler(sig: libc::c_int) {
    let ptr = CLEANUP_CSTRING.swap(std::ptr::null_mut(), Ordering::SeqCst);
    if !ptr.is_null() {
        // libc::unlink is guaranteed async-signal-safe by POSIX
        unsafe {
            libc::unlink(ptr);
        }
    }
    // Re-raise the signal with default handler to exit with standard signal exit code
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

/// RAII Guard that guarantees temporary file removal on normal exit, error, or panic.
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
// 2. MEMORY-ALIGNED BUFFER (Required for Linux O_DIRECT)
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
// 3. FAST 64-BIT PRNG (Zero external crates, high entropy, no modulo bias)
// ==============================================================================

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
// 4. FORMATTING & UI (Matches diskbar aesthetic)
// ==============================================================================

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

struct BenchmarkResult {
    test_name: String,
    block_size_str: String,
    transferred_str: String,
    elapsed_secs: f64,
    speed_mb_s: f64,
}

// ==============================================================================
// 5. SECURITY & KERNEL SYSCALL HELPERS
// ==============================================================================

/// Checks free space on the destination mount via libc statvfs syscall.
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

/// Generates a non-predictable temporary filename with 64-bit random entropy.
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

/// Creates a new benchmark file with O_CREAT | O_EXCL and restricted mode 0600.
/// Tries O_DIRECT first; falls back cleanly if filesystem does not support it.
fn create_test_file(path: &Path, use_direct: bool) -> io::Result<(File, bool)> {
    if use_direct {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create_new(true).mode(0o600);
        opts.custom_flags(libc::O_DIRECT);
        if let Ok(file) = opts.open(path) {
            return Ok((file, true));
        }
    }

    // Fallback: standard buffered I/O
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create_new(true).mode(0o600);
    let file = opts.open(path)?;
    Ok((file, false))
}

/// Opens an existing test file for read or write.
fn open_existing_file(path: &Path, write: bool, direct: bool) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(write);
    if direct {
        opts.custom_flags(libc::O_DIRECT);
    }
    opts.open(path)
}

/// Drops clean cached pages from the OS page cache for this file.
fn drop_os_cache(file: &File) {
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

// ==============================================================================
// 6. BENCHMARK RUNNERS
// ==============================================================================

fn run_seq_write(
    file: &mut File,
    total_bytes: usize,
    chunk_size: usize,
    is_tty: bool,
) -> io::Result<BenchmarkResult> {
    let mut buffer = AlignedBuffer::new(chunk_size, 4096);
    // Fill with non-zero dummy pattern
    for (i, b) in buffer.as_mut_slice().iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }

    let total_chunks = total_bytes / chunk_size;
    let start_time = Instant::now();
    let mut last_update = Instant::now();

    for i in 0..total_chunks {
        file.write_all(buffer.as_slice())?;

        let is_last = i + 1 == total_chunks;
        if is_tty && (last_update.elapsed().as_millis() >= 40 || is_last) {
            let done_bytes = (i + 1) * chunk_size;
            let pct = ((done_bytes as f64 / total_bytes as f64) * 100.0) as u8;
            let elapsed = start_time.elapsed().as_secs_f64().max(0.0001);
            let current_mb = done_bytes as f64 / (1024.0 * 1024.0);
            let speed = current_mb / elapsed;

            let bar = make_bar(pct, 20, "\x1b[36m"); // Cyan
            print!(
                "\r\x1b[2K  ▶ \x1b[1m{:<14}\x1b[0m {} {:>3}%  {:>11}  ({:.1}s)",
                "Seq Write", bar, pct, format_speed(speed), elapsed
            );
            io::stdout().flush()?;
            last_update = Instant::now();
        }
    }

    // Ensure all physical bytes are committed to media
    file.sync_all()?;
    let elapsed = start_time.elapsed().as_secs_f64().max(0.0001);
    let speed_mb_s = (total_bytes as f64 / (1024.0 * 1024.0)) / elapsed;

    let bar = make_bar(100, 20, "\x1b[36m");
    if is_tty {
        println!(
            "\r\x1b[2K  ✔ \x1b[1m{:<14}\x1b[0m {} 100%  {:>11}  ({:.2}s)",
            "Seq Write", bar, format_speed(speed_mb_s), elapsed
        );
    } else {
        println!(
            "  ✔ {:<14} {} 100%  {:>11}  ({:.2}s)",
            "Seq Write", bar, format_speed(speed_mb_s), elapsed
        );
    }

    Ok(BenchmarkResult {
        test_name: "Seq Write".to_string(),
        block_size_str: format_bytes(chunk_size as u64),
        transferred_str: format_bytes(total_bytes as u64),
        elapsed_secs: elapsed,
        speed_mb_s,
    })
}

fn run_seq_read(
    path: &Path,
    total_bytes: usize,
    chunk_size: usize,
    use_direct: bool,
    is_tty: bool,
) -> io::Result<BenchmarkResult> {
    let mut file = open_existing_file(path, false, use_direct)?;
    if !use_direct {
        drop_os_cache(&file);
    }

    let mut buffer = AlignedBuffer::new(chunk_size, 4096);
    let total_chunks = total_bytes / chunk_size;
    let start_time = Instant::now();
    let mut last_update = Instant::now();

    for i in 0..total_chunks {
        file.read_exact(buffer.as_mut_slice())?;

        let is_last = i + 1 == total_chunks;
        if is_tty && (last_update.elapsed().as_millis() >= 40 || is_last) {
            let done_bytes = (i + 1) * chunk_size;
            let pct = ((done_bytes as f64 / total_bytes as f64) * 100.0) as u8;
            let elapsed = start_time.elapsed().as_secs_f64().max(0.0001);
            let current_mb = done_bytes as f64 / (1024.0 * 1024.0);
            let speed = current_mb / elapsed;

            let bar = make_bar(pct, 20, "\x1b[32m"); // Green
            print!(
                "\r\x1b[2K  ▶ \x1b[1m{:<14}\x1b[0m {} {:>3}%  {:>11}  ({:.1}s)",
                "Seq Read", bar, pct, format_speed(speed), elapsed
            );
            io::stdout().flush()?;
            last_update = Instant::now();
        }
    }

    let elapsed = start_time.elapsed().as_secs_f64().max(0.0001);
    let speed_mb_s = (total_bytes as f64 / (1024.0 * 1024.0)) / elapsed;

    let bar = make_bar(100, 20, "\x1b[32m");
    if is_tty {
        println!(
            "\r\x1b[2K  ✔ \x1b[1m{:<14}\x1b[0m {} 100%  {:>11}  ({:.2}s)",
            "Seq Read", bar, format_speed(speed_mb_s), elapsed
        );
    } else {
        println!(
            "  ✔ {:<14} {} 100%  {:>11}  ({:.2}s)",
            "Seq Read", bar, format_speed(speed_mb_s), elapsed
        );
    }

    Ok(BenchmarkResult {
        test_name: "Seq Read".to_string(),
        block_size_str: format_bytes(chunk_size as u64),
        transferred_str: format_bytes(total_bytes as u64),
        elapsed_secs: elapsed,
        speed_mb_s,
    })
}

fn run_rnd_write_4k(
    path: &Path,
    file_size: usize,
    test_bytes: usize,
    use_direct: bool,
    is_tty: bool,
) -> io::Result<BenchmarkResult> {
    let file = open_existing_file(path, true, use_direct)?;
    let block_size = 4096;
    let mut buffer = AlignedBuffer::new(block_size, 4096);
    for (i, b) in buffer.as_mut_slice().iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }

    let num_blocks = file_size / block_size;
    if num_blocks == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "File too small for 4K test"));
    }

    let total_ops = test_bytes / block_size;
    let mut rng = SimpleRng::new(0xDEADBEEF);

    let start_time = Instant::now();
    let mut last_update = Instant::now();

    for i in 0..total_ops {
        let block_idx = (rng.next_u64() as usize) % num_blocks;
        let offset = (block_idx * block_size) as u64;

        // Atomic positional write without lseek syscall
        file.write_all_at(buffer.as_slice(), offset)?;

        let is_last = i + 1 == total_ops;
        // Throttle clock checks to every 64 iterations to eliminate CPU bottleneck
        if is_tty && (i & 63 == 0 || is_last) && (last_update.elapsed().as_millis() >= 40 || is_last) {
            let done_bytes = (i + 1) * block_size;
            let pct = ((done_bytes as f64 / test_bytes as f64) * 100.0) as u8;
            let elapsed = start_time.elapsed().as_secs_f64().max(0.0001);
            let current_mb = done_bytes as f64 / (1024.0 * 1024.0);
            let speed = current_mb / elapsed;

            let bar = make_bar(pct, 20, "\x1b[33m"); // Yellow
            print!(
                "\r\x1b[2K  ▶ \x1b[1m{:<14}\x1b[0m {} {:>3}%  {:>11}  ({:.1}s)",
                "Rnd 4K Write", bar, pct, format_speed(speed), elapsed
            );
            io::stdout().flush()?;
            last_update = Instant::now();
        }
    }

    file.sync_all()?;
    let elapsed = start_time.elapsed().as_secs_f64().max(0.0001);
    let speed_mb_s = (test_bytes as f64 / (1024.0 * 1024.0)) / elapsed;

    let bar = make_bar(100, 20, "\x1b[33m");
    if is_tty {
        println!(
            "\r\x1b[2K  ✔ \x1b[1m{:<14}\x1b[0m {} 100%  {:>11}  ({:.2}s)",
            "Rnd 4K Write", bar, format_speed(speed_mb_s), elapsed
        );
    } else {
        println!(
            "  ✔ {:<14} {} 100%  {:>11}  ({:.2}s)",
            "Rnd 4K Write", bar, format_speed(speed_mb_s), elapsed
        );
    }

    Ok(BenchmarkResult {
        test_name: "Rnd 4K Write".to_string(),
        block_size_str: format_bytes(block_size as u64),
        transferred_str: format_bytes(test_bytes as u64),
        elapsed_secs: elapsed,
        speed_mb_s,
    })
}

fn run_rnd_read_4k(
    path: &Path,
    file_size: usize,
    test_bytes: usize,
    use_direct: bool,
    is_tty: bool,
) -> io::Result<BenchmarkResult> {
    let file = open_existing_file(path, false, use_direct)?;
    if !use_direct {
        drop_os_cache(&file);
    }

    let block_size = 4096;
    let mut buffer = AlignedBuffer::new(block_size, 4096);
    let num_blocks = file_size / block_size;
    if num_blocks == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "File too small for 4K test"));
    }

    let total_ops = test_bytes / block_size;
    let mut rng = SimpleRng::new(0xCAFEBABE);

    let start_time = Instant::now();
    let mut last_update = Instant::now();

    for i in 0..total_ops {
        let block_idx = (rng.next_u64() as usize) % num_blocks;
        let offset = (block_idx * block_size) as u64;

        // Atomic positional read without lseek syscall
        file.read_exact_at(buffer.as_mut_slice(), offset)?;

        let is_last = i + 1 == total_ops;
        // Throttle clock checks to every 64 iterations
        if is_tty && (i & 63 == 0 || is_last) && (last_update.elapsed().as_millis() >= 40 || is_last) {
            let done_bytes = (i + 1) * block_size;
            let pct = ((done_bytes as f64 / test_bytes as f64) * 100.0) as u8;
            let elapsed = start_time.elapsed().as_secs_f64().max(0.0001);
            let current_mb = done_bytes as f64 / (1024.0 * 1024.0);
            let speed = current_mb / elapsed;

            let bar = make_bar(pct, 20, "\x1b[35m"); // Magenta
            print!(
                "\r\x1b[2K  ▶ \x1b[1m{:<14}\x1b[0m {} {:>3}%  {:>11}  ({:.1}s)",
                "Rnd 4K Read", bar, pct, format_speed(speed), elapsed
            );
            io::stdout().flush()?;
            last_update = Instant::now();
        }
    }

    let elapsed = start_time.elapsed().as_secs_f64().max(0.0001);
    let speed_mb_s = (test_bytes as f64 / (1024.0 * 1024.0)) / elapsed;

    let bar = make_bar(100, 20, "\x1b[35m");
    if is_tty {
        println!(
            "\r\x1b[2K  ✔ \x1b[1m{:<14}\x1b[0m {} 100%  {:>11}  ({:.2}s)",
            "Rnd 4K Read", bar, format_speed(speed_mb_s), elapsed
        );
    } else {
        println!(
            "  ✔ {:<14} {} 100%  {:>11}  ({:.2}s)",
            "Rnd 4K Read", bar, format_speed(speed_mb_s), elapsed
        );
    }

    Ok(BenchmarkResult {
        test_name: "Rnd 4K Read".to_string(),
        block_size_str: format_bytes(block_size as u64),
        transferred_str: format_bytes(test_bytes as u64),
        elapsed_secs: elapsed,
        speed_mb_s,
    })
}

// ==============================================================================
// 7. CLI ARGUMENT PARSER
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
    println!("--------------------------------------------------------------------------------------");
    println!("Usage: diskbench [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -p, --path <DIR>       Directory or mount path to benchmark (default: current dir)");
    println!("  -s, --size <SIZE>      Total test file size (e.g. 256M, 512M, 1G, 2G; default: 512M)");
    println!("  -a, --all              Run both Sequential and Random 4K benchmark tests");
    println!("      --no-direct        Disable O_DIRECT (use OS page cache + fsync)");
    println!("  -h, --help             Show this help screen");
    println!("--------------------------------------------------------------------------------------");
    println!();
}

// ==============================================================================
// 8. MAIN ENTRYPOINT
// ==============================================================================

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut target_dir = PathBuf::from(".");
    let mut requested_bytes = 512 * 1024 * 1024; // 512 MB default
    let mut run_all_tests = false;
    let mut use_direct = true;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_help();
                return;
            }
            "-a" | "--all" => {
                run_all_tests = true;
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

    // 2. Canonicalize path for display
    let display_path = target_dir.canonicalize().unwrap_or_else(|_| target_dir.clone());
    let path_str = display_path.to_string_lossy();

    // 3. Align size to 1MB chunks (minimum 4MB)
    let chunk_size = 1024 * 1024; // 1 MB
    let min_bytes = 4 * 1024 * 1024;
    let total_bytes = requested_bytes.max(min_bytes).div_ceil(chunk_size) * chunk_size;

    // 4. Check available storage space before writing
    if let Some(avail_bytes) = get_available_bytes(&target_dir) {
        let margin_bytes = 50 * 1024 * 1024; // 50 MB safety margin
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

    // 5. Generate secure non-predictable temporary file path
    let test_file_path = generate_secure_temp_path(&target_dir);

    // 6. Register RAII cleanup guard to guarantee removal on drop / panic
    let _guard = TempFileGuard::new(test_file_path.clone());

    // 7. Probe and create the benchmark test file
    let (mut write_file, is_direct_effective) = match create_test_file(&test_file_path, use_direct) {
        Ok(res) => res,
        Err(e) => {
            eprintln!("❌ Failed to create benchmark test file: {}", e);
            std::process::exit(1);
        }
    };

    let width = 86;
    let divider = "-".repeat(width);
    let is_tty = is_stdout_tty();

    println!();
    println!("⚡ \x1b[1mDISK PERFORMANCE BENCHMARK (Direct Kernel I/O)\x1b[0m");
    println!("{}", divider);
    println!(
        "\x1b[1mTARGET:\x1b[0m {:<22} | \x1b[1mSIZE:\x1b[0m {:<6} | \x1b[1mSEQ BLOCK:\x1b[0m 1.0M | \x1b[1mDIRECT I/O:\x1b[0m {}",
        if path_str.len() > 22 { &path_str[..22] } else { &path_str },
        format_bytes(total_bytes as u64),
        if is_direct_effective {
            "\x1b[32mENABLED (O_DIRECT)\x1b[0m"
        } else if use_direct {
            "\x1b[33mFALLBACK (fsync)\x1b[0m"
        } else {
            "\x1b[33mDISABLED\x1b[0m"
        }
    );

    if use_direct && !is_direct_effective {
        println!("\x1b[33m⚠️  Notice: Filesystem rejected O_DIRECT. Using buffered I/O with page cache flushing.\x1b[0m");
    }

    println!("{}", divider);
    println!("Running benchmark tests...\n");

    let mut results: Vec<BenchmarkResult> = Vec::new();

    // Test 1: Sequential Write
    match run_seq_write(&mut write_file, total_bytes, chunk_size, is_tty) {
        Ok(res) => results.push(res),
        Err(e) => {
            eprintln!("\n❌ Sequential Write Failed: {}", e);
            std::process::exit(1);
        }
    }
    drop(write_file); // Close write descriptor before read phase

    // Test 2: Sequential Read
    match run_seq_read(&test_file_path, total_bytes, chunk_size, is_direct_effective, is_tty) {
        Ok(res) => results.push(res),
        Err(e) => {
            eprintln!("\n❌ Sequential Read Failed: {}", e);
        }
    }

    // Optional Test 3 & 4: Random 4K Operations
    if run_all_tests {
        let rnd_bytes = (total_bytes / 8).clamp(8 * 1024 * 1024, 64 * 1024 * 1024);

        match run_rnd_write_4k(&test_file_path, total_bytes, rnd_bytes, is_direct_effective, is_tty) {
            Ok(res) => results.push(res),
            Err(e) => eprintln!("\n❌ Random 4K Write Failed: {}", e),
        }

        match run_rnd_read_4k(&test_file_path, total_bytes, rnd_bytes, is_direct_effective, is_tty) {
            Ok(res) => results.push(res),
            Err(e) => eprintln!("\n❌ Random 4K Read Failed: {}", e),
        }
    }

    // Note: _guard drops here, automatically unlinking test_file_path.

    // 8. Render summary table (matching diskbar visual styling)
    println!();
    println!("📊 \x1b[1mBENCHMARK SUMMARY RESULTS\x1b[0m");
    println!("{}", divider);
    println!(
        "\x1b[1m{:<16} {:>10} {:>12} {:>8} {:>14}   {:<24} {:>6}\x1b[0m",
        "TEST TYPE", "BLOCK SIZE", "TRANSFERRED", "TIME", "SPEED", "PERFORMANCE BAR", "RATING"
    );
    println!("{}", divider);

    let max_speed = results
        .iter()
        .map(|r| r.speed_mb_s)
        .fold(1.0, f64::max);

    for r in &results {
        let score_pct = if max_speed > 0.0 {
            ((r.speed_mb_s / max_speed) * 100.0).clamp(5.0, 100.0) as u8
        } else {
            5
        };
        let bar = make_bar(score_pct, 18, "\x1b[32m");
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
            "{:<16} {:>10} {:>12} {:>7.2}s {:>14}   {} {:>6}",
            r.test_name,
            r.block_size_str,
            r.transferred_str,
            r.elapsed_secs,
            format_speed(r.speed_mb_s),
            bar,
            rating
        );
    }

    println!("{}", divider);
    if !run_all_tests {
        println!(
            "\x1b[90mTip: Run with --all (or -a) to include Random 4K Read/Write tests.\x1b[0m\n"
        );
    } else {
        println!();
    }
}

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
}
