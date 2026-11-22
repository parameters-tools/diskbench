<div align="center">

```text
██████╗ ██╗███████╗██╗  ██╗██████╗ ███████╗███╗   ██╗ ██████╗██╗  ██╗
██╔══██╗██║██╔════╝██║ ██╔╝██╔══██╗██╔════╝████╗  ██║██╔════╝██║  ██║
██║  ██║██║███████╗█████╔╝ ██████╔╝█████╗  ██╔██╗ ██║██║     ███████║
██║  ██║██║╚════██║██╔═██╗ ██╔══██╗██╔══╝  ██║╚██╗██║██║     ██╔══██║
██████╔╝██║███████║██║  ██╗██████╔╝███████╗██║ ╚████║╚██████╗██║  ██║
╚═════╝ ╚═╝╚══════╝╚═╝  ╚═╝╚═════╝ ╚══════╝╚═╝  ╚═══╝ ╚═════╝╚═╝  ╚═╝
```

# ⚡ DISKBENCH

**An industry-grade disk performance benchmark CLI with real-time terminal sparklines, queue-depth concurrency, hardware telemetry, thermal monitoring, and direct Linux kernel I/O (`O_DIRECT`). Styled after [`diskbar`](https://github.com/parameters-tools/diskbar).**

[![Rust](https://img.shields.io/badge/rust-%23000000.svg?style=for-the-badge&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Linux](https://img.shields.io/badge/linux-%23FCC624.svg?style=for-the-badge&logo=linux&logoColor=black)](https://www.kernel.org/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg?style=for-the-badge)](#-license)

[Features](#-features) •
[One-Line Install](#-one-line-install) •
[Usage](#-usage) •
[CrystalDiskMark Profile Suite](#-crystaldiskmark-profile-suite) •
[Baseline Comparison](#-baseline-comparative-benchmarking) •
[Hardware Telemetry](#-hardware-telemetry--thermal-monitoring) •
[Security & Safety](#-security--safety-guarantees) •
[License](#-license)

</div>

---

```text
⚡ DISK PERFORMANCE BENCHMARK (Direct Kernel I/O)
--------------------------------------------------------------------------------------------------
TARGET: /home/kevin            | MODEL: SK hynix PC711 HFS512GDE9X073N      
TRANS:  NVMe (PCIe)            | PCIE:  5.0 GT/s PCIe x4 | TEMP: 42.8°C | SIZE: 1.0G  
DIRECT I/O: ENABLED (O_DIRECT)
--------------------------------------------------------------------------------------------------
Running benchmark tests...

  ✔ SEQ1M Q8T1 Write   [████████████████] 100%    2.48 GB/s  [p95: 8.30 ms]  (0.42s)
  ✔ SEQ1M Q8T1 Read    [████████████████] 100%    2.53 GB/s  [p95: 4.43 ms]  (0.40s)
  ✔ SEQ1M Q1T1 Write   [████████████████] 100%    1.85 GB/s  [p95:  729 µs]  (0.55s)
  ✔ SEQ1M Q1T1 Read    [████████████████] 100%    2.10 GB/s  [p95:  688 µs]  (0.49s)
  ✔ RND4K Q32T16 Write [████████████████] 100%    1.17 GB/s  [p95:   63 µs]  (0.08s)
  ✔ RND4K Q32T16 Read  [████████████████] 100%   996.0 MB/s  [p95:   82 µs]  (0.10s)
  ✔ RND4K Q1T1 Write   [████████████████] 100%   142.1 MB/s  [p95:   39 µs]  (0.22s)
  ✔ RND4K Q1T1 Read    [████████████████] 100%    71.4 MB/s  [p95:   76 µs]  (0.45s)

📊 BENCHMARK SUMMARY RESULTS
--------------------------------------------------------------------------------------------------
TEST PROFILE                SPEED       IOPS  LAT (p95)   PERFORMANCE BAR      RATING  THREADS
--------------------------------------------------------------------------------------------------
SEQ1M Q8T1 Write        2.48 GB/s       2.5k    8.30 ms   [██████████████] ULTRA       8
SEQ1M Q8T1 Read         2.53 GB/s       2.6k    4.43 ms   [██████████████] ULTRA       8
SEQ1M Q1T1 Write        1.85 GB/s       1.9k     729 µs   [██████████░░░░] FAST        1
SEQ1M Q1T1 Read         2.10 GB/s       2.1k     688 µs   [███████████░░░] ULTRA       1
RND4K Q32T16 Write      1.17 GB/s     307.5k      63 µs   [██████░░░░░░░░] FAST       16
RND4K Q32T16 Read      996.0 MB/s     255.0k      82 µs   [█████░░░░░░░░░] FAST       16
RND4K Q1T1 Write       142.1 MB/s      36.4k      39 µs   [█░░░░░░░░░░░░░] GOOD        1
RND4K Q1T1 Read         71.4 MB/s      18.3k      76 µs   [░░░░░░░░░░░░░░] SLOW        1
--------------------------------------------------------------------------------------------------
Thermal status: 43.1°C (initial: 42.8°C, Δ: +0.3°C)
```

---

## ✨ Features

- ⚡ **Direct Kernel I/O (`O_DIRECT`):** Bypasses the OS page cache using 4096-byte memory-aligned DMA buffers. Measures true physical NVMe, SSD, and HDD hardware throughput rather than RAM speeds.
- 🧵 **Multi-Threaded Queue Concurrency:** Saturates PCIe Gen4/Gen5 NVMe submission queues with multi-worker threads (`SEQ1M Q8T1`, `RND4K Q32T16`) to benchmark peak hardware bandwidth and IOPS.
- ⏱️ **Microsecond Latency Profiling:** Records granular execution times for every I/O operation. Calculates Average, Min, Max, and critical **95th percentile (p95) & 99th percentile (p99)** latency metrics.
- 🌡️ **Hardware Telemetry & Thermal Guard:** Automatically queries Linux sysfs to report drive model, transport interface (`NVMe`, `SATA`, `USB`), PCIe link speed & lane width (`5.0 GT/s PCIe x4`), and active drive temperature. Warns if thermal throttling occurs during the test.
- 📊 **Real-time ASCII Sparklines:** Displays an interactive live throughput sparkline (` ▂▃▄▅▆▇█`) during test execution.
- ⚖️ **Baseline Comparison Mode (`--compare`):** Compare benchmark runs against a saved baseline with colored differential indicators (`+16.7% ▲` or `-3.2% ▼`).
- 🔄 **Multi-Pass Runs (`--runs`):** Run multiple passes with automatic statistical averaging.
- 🤖 **Machine-Readable JSON (`--json` & `--save`):** Plug results directly into CI/CD performance regression pipelines or automation scripts.
- 🛡️ **Built-in Safety Guardrails:**
  - Pre-flight `statvfs` capacity check prevents `ENOSPC` disk exhaustion.
  - Symlink-hardened temporary file creation (`O_CREAT | O_EXCL` with mode `0600`).
  - Async-signal-safe RAII cleanup on `SIGINT` (Ctrl+C) and `SIGTERM`.
- 🪶 **Standalone Single Binary:** Pure Rust Standard Library + `libc`. Only ~450 KB with zero external dependencies.

---

## 🚀 One-Line Install

Install immediately with a single command:

```bash
curl -sSL https://raw.githubusercontent.com/parameters-tools/diskbench/main/install.sh | bash
```

### Alternative Installation Methods

#### Via Cargo (from GitHub)
```bash
cargo install --git https://github.com/parameters-tools/diskbench
```

#### Build from Source
```bash
git clone https://github.com/parameters-tools/diskbench.git
cd diskbench
cargo build --release
sudo install -m 755 target/release/diskbench /usr/local/bin/diskbench
```

---

## 📖 Usage

### Standard Quick Benchmark (512 MB)
```bash
diskbench
```

### Complete CrystalDiskMark Profile Suite
```bash
diskbench --all
# or
diskbench -a
```

### Benchmark with Custom File Size & Threads
```bash
# Benchmark with 1 GB file using 4 worker threads
diskbench --size 1G --threads 4

# Benchmark a specific mount or external SSD
diskbench --path /mnt/ssd --size 2G --all
```

### Multi-Pass Averaging
```bash
# Run 3 benchmark iterations and compute average metrics
diskbench --runs 3
```

### Baseline Saving & Comparison Mode
```bash
# 1. Save baseline before tuning
diskbench --size 1G --save baseline.json

# 2. Run comparative test after driver/kernel/filesystem changes
diskbench --size 1G --compare baseline.json
```

```text
TEST PROFILE             BASELINE  CURRENT SPEED     DIFF %   PERFORMANCE BAR      LAT (p95)
--------------------------------------------------------------------------------------------------
Seq Write               1.85 GB/s      2.12 GB/s +14.6% ▲   [██████████████]   680 µs
Seq Read                2.10 GB/s      2.18 GB/s  +3.8% ▲   [██████████████]   655 µs
Rnd 4K Write           142.1 MB/s     151.3 MB/s  +6.5% ▲   [█░░░░░░░░░░░░░]    36 µs
Rnd 4K Read             71.4 MB/s      69.8 MB/s  -2.2% ▼   [░░░░░░░░░░░░░░]    78 µs
--------------------------------------------------------------------------------------------------
```

### Machine-Readable JSON Export
```bash
diskbench --size 512M --json | jq .
```

---

## 📊 CrystalDiskMark Profile Suite

Running with `--all` tests the 4 standard industry benchmark profiles:

| Profile | Block Size | Concurrency | Real-World Workload Simulated |
| :--- | :--- | :--- | :--- |
| **`SEQ1M Q8T1`** | 1 MB | 8 Threads | Peak sustained streaming bandwidth (large movie/ISO transfers, video editing). |
| **`SEQ1M Q1T1`** | 1 MB | 1 Thread | Standard single-stream file read/write performance. |
| **`RND4K Q32T16`** | 4 KB | 16 Threads | Peak drive IOPS under heavy multi-process server / database load. |
| **`RND4K Q1T1`** | 4 KB | 1 Thread | **Single-queue low latency**: Dictates app launches, OS boot responsiveness, and general snappiness. |

---

## 🛠️ Hardware Telemetry & Thermal Monitoring

`diskbench` queries the Linux kernel's sysfs tree (`/sys/dev/block/`, `/sys/class/nvme/`, and `/sys/class/hwmon/`) to provide hardware context alongside speed metrics:
- **Device Model:** Extracted from device descriptors (e.g. `Samsung SSD 990 PRO 2TB`, `SK hynix PC711`).
- **Transport Interface:** Identifies NVMe over PCIe, SATA/SCSI, eMMC, or USB.
- **PCIe Link Speed & Lane Count:** Verifies if the drive operates at full capacity (e.g. `16.0 GT/s PCIe x4` vs degraded `x2` lanes).
- **Active Thermal Monitoring:** Records operating temperature before and after testing. Warns with `🔥 WARNING` if drive temperature reaches 70°C or thermal throttling limits.

---

## 🛡️ Security & Safety Guarantees

- **Symlink & Race Prevention (CWE-59 / CWE-377):** Files are created with `O_CREAT | O_EXCL` using 64-bit entropy from `/dev/urandom`. It will never follow a symlink or overwrite an existing file.
- **Restricted Permissions (CWE-732):** Test files are created with mode `0600` (read/write by owner only).
- **Disk Full Protection:** Queries `libc::statvfs` before writing a single byte. If the mount lacks the requested space plus a 50 MB safety margin, it aborts cleanly.
- **Signal-Safe Cleanup:** If you cancel the benchmark midway with `Ctrl+C`, an async-signal-safe POSIX handler immediately unlinks the temporary file so your drive never fills up with orphaned test files.

---

## 📄 License

Dual-licensed under either the [MIT License](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option.
