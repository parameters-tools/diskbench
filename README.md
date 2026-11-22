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

**A blazing-fast disk speed benchmark CLI with real-time terminal progress indicators and direct Linux kernel I/O (`O_DIRECT`). Styled after [`diskbar`](https://github.com/parameters-tools/diskbar).**

[![Rust](https://img.shields.io/badge/rust-%23000000.svg?style=for-the-badge&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Linux](https://img.shields.io/badge/linux-%23FCC624.svg?style=for-the-badge&logo=linux&logoColor=black)](https://www.kernel.org/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg?style=for-the-badge)](#-license)

[Features](#-features) •
[One-Line Install](#-one-line-install) •
[Usage](#-usage) •
[How It Works](#-how-it-works) •
[Security & Safety](#-security--safety-guarantees) •
[License](#-license)

</div>

---

```text
⚡ DISK PERFORMANCE BENCHMARK (Direct Kernel I/O)
--------------------------------------------------------------------------------------
TARGET: /home/kevin            | SIZE: 512M   | SEQ BLOCK: 1.0M | DIRECT I/O: ENABLED (O_DIRECT)
--------------------------------------------------------------------------------------
Running benchmark tests...

  ✔ Seq Write      [████████████████████] 100%    2.48 GB/s  (0.21s)
  ✔ Seq Read       [████████████████████] 100%    2.25 GB/s  (0.23s)
  ✔ Rnd 4K Write   [████████████████████] 100%   142.1 MB/s  (0.45s)
  ✔ Rnd 4K Read    [████████████████████] 100%    71.4 MB/s  (0.90s)

📊 BENCHMARK SUMMARY RESULTS
--------------------------------------------------------------------------------------
TEST TYPE        BLOCK SIZE  TRANSFERRED     TIME          SPEED   PERFORMANCE BAR          RATING
--------------------------------------------------------------------------------------
Seq Write              1.0M         512M    0.21s      2.48 GB/s   [██████████████████] ULTRA
Seq Read               1.0M         512M    0.23s      2.25 GB/s   [████████████████░░] ULTRA
Rnd 4K Write             4K          64M    0.45s     142.1 MB/s   [█░░░░░░░░░░░░░░░░░] GOOD
Rnd 4K Read              4K          64M    0.90s      71.4 MB/s   [░░░░░░░░░░░░░░░░░░] SLOW
--------------------------------------------------------------------------------------
```

---

## ✨ Features

- ⚡ **Direct Kernel I/O (`O_DIRECT`):** Bypasses the OS page cache using 4096-byte memory-aligned DMA buffers. Measures true physical NVMe, SSD, and HDD hardware throughput rather than RAM speeds.
- 🎨 **Visual Aesthetic Matching `diskbar`:** Smooth Unicode progress bars (`█` / `░`), color-coded status ratings, and clean column layout.
- 🏎️ **Positional Atomic I/O (`pread` / `pwrite`):** Random 4K operations execute in a single syscall via `FileExt`, avoiding `lseek` thrashing and cutting syscall overhead by half.
- 🔄 **Comprehensive Testing:**
  - **Sequential Write & Read** (1 MB blocks) for sustained high-throughput transfer.
  - **Random 4K Write & Read** for IOPS simulation (OS boot, apps, databases).
- 🛡️ **Built-in Safety Guardrails:**
  - Pre-flight `statvfs` disk space check prevents `ENOSPC` disk exhaustion.
  - Symlink-hardened temporary file creation (`O_CREAT | O_EXCL` with mode `0600`).
  - Async-signal-safe RAII cleanup on `SIGINT` (Ctrl+C) and `SIGTERM`.
- 🪶 **Zero Heavy Crates:** Pure Rust Standard Library + `libc`. Extremely lightweight with sub-second compile times.

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

### Standard Sequential Test (512 MB)
```bash
diskbench
```

### Full Benchmark Suite (Sequential + Random 4K)
```bash
diskbench --all
# or
diskbench -a
```

### Benchmark a Specific Mount / Drive with Custom Size
```bash
# Benchmark an external SSD or mount with a 1 GB file
diskbench --path /mnt/drive --size 1G --all

# Test with 256 MB test size
diskbench --size 256M
```

### CLI Options

```text
Usage: diskbench [OPTIONS]

Options:
  -p, --path <DIR>       Directory or mount path to benchmark (default: current dir)
  -s, --size <SIZE>      Total test file size (e.g. 256M, 512M, 1G, 2G; default: 512M)
  -a, --all              Run both Sequential and Random 4K benchmark tests
      --no-direct        Disable O_DIRECT (use OS page cache + fsync)
  -h, --help             Show this help screen
```

---

## 🛠️ How It Works

### The Cache Bypass Problem
When an application writes a file, modern operating systems store it in the **RAM page cache** before flushing to disk. Reading the file back often retrieves it straight from RAM, producing misleading speeds (e.g., 15+ GB/s on an old hard drive).

`diskbench` solves this directly at the kernel boundary:
1. **DMA Buffer Alignment:** Allocates heap memory aligned to 4096-byte hardware boundaries (`std::alloc::Layout`).
2. **`O_DIRECT` Syscalls:** Opens temporary test files with the Linux kernel's `O_DIRECT` flag, forcing storage controller DMA directly to/from user space memory.
3. **Hardware Barrier (`fsync`):** Commits all pending controller queues to non-volatile physical storage before stopping the benchmark stopwatch.
4. **Automatic Fallback:** If run on filesystems that disallow direct I/O (e.g., tmpfs or overlayfs), `diskbench` automatically falls back to standard I/O with `posix_fadvise(..., POSIX_FADV_DONTNEED)` cache invalidation and notifies you.

---

## 🛡️ Security & Safety Guarantees

- **Symlink & Race Prevention (CWE-59 / CWE-377):** Files are created with `O_CREAT | O_EXCL` using 64-bit entropy from `/dev/urandom`. It will never follow a symlink or overwrite an existing file.
- **Restricted Permissions (CWE-732):** Test files are created with mode `0600` (read/write by owner only).
- **Disk Full Protection:** Queries `libc::statvfs` before writing a single byte. If the mount lacks the requested space plus a 50 MB safety margin, it aborts cleanly.
- **Signal-Safe Cleanup:** If you cancel the benchmark midway with `Ctrl+C`, an async-signal-safe POSIX handler immediately unlinks the temporary file so your drive never fills up with orphaned test files.

---

## 📄 License

Dual-licensed under either the [MIT License](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option.
