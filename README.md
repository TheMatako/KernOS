# KernOS

> A security-oriented, bare-metal operating system built from scratch in Rust.

KernOS is a learning-driven OS development project built brick by brick, with full inline documentation so that anyone — even without prior OS development experience — can follow and understand every design decision.

The long-term goal is a lightweight, fast-booting kernel with built-in network and cybersecurity tooling at the OS level, running on real x86_64 hardware without any dependency on an existing OS or runtime.

---

## Current Status

| Component                          | Status        |
|------------------------------------|---------------|
| UEFI Bootloader                    | ✅ Complete   |
| PXE Network Boot                   | ✅ Complete   |
| Brick 1 — Bootstrap + Serial       | ✅ Complete   |
| Brick 2 — GDT / IDT / Interrupts   | ✅ Complete   |
| Brick 3 — Physical Memory Manager  | ✅ Complete   |
| Brick 4 — VMM / Paging / Slab      | ✅ Complete   |
| Brick 5 — Preemptive Scheduler     | ✅ Complete   |
| Brick 6 — Syscall / Ring-3         | ✅ Complete   |
| Brick 7 — Drivers (KBD/PCI/Block)  | ✅ Complete   |
| Brick 8 — KernFS / VFS             | ✅ Complete   |
| Brick 9 — TCP/IP Stack + e1000     | ✅ Complete   |
| Brick 10 — Interactive Shell       | ✅ Complete   |
| Brick 11 — GOP Framebuffer         | ✅ Complete   |
|----------------------------------------------------|

This represents KernOS v1.0.0

---

## What Works Right Now

- **Boots on real hardware** via PXE (tested on Dell Latitude 7340) and from disk via QEMU
- **Full graphical text console** using the UEFI GOP framebuffer with `font8x8` — no serial cable needed on real hardware
- **Interactive shell** with 17 built-in commands (`ls`, `cat`, `ping`, `ps`, `netstat`, `memstat`, …)
- **KernFS** — a custom RAM filesystem optimised for speed (extents, flat inode table, 4 KiB blocks aligned on PMM frames)
- **Complete TCP/IP stack** — Ethernet → ARP → IPv4 → ICMP → UDP → TCP with automatic ICMP echo reply (ping)
- **Preemptive scheduler** at ~100 Hz driven by the Local APIC timer, with a pure-assembly context switch
- **Syscall interface** via `syscall`/`sysret` MSRs, compatible with the musl libc ABI
- **AZERTY-FR keyboard** with Shift, AltGr and Ctrl modifier layers

---

## Architecture

```
UEFI Firmware
    └── Bootloader (Rust, x86_64-unknown-uefi)
          ├── Loads kernel.elf from FAT32 partition or via PXE/TFTP
          ├── Parses ELF segments
          ├── Fills BootInfo (memory map, framebuffer GOP, RSDP, kernel location)
          ├── Exits UEFI boot services
          └── Jumps to kernel_main(*const BootInfo)

Kernel (Rust + minimal ASM, x86_64-unknown-none)
    ├── Bootstrap      — zero BSS, UART 16550 serial @ 115200 baud
    ├── GDT            — kernel/user segments, TSS + IST stack for #DF
    ├── IDT            — 32 CPU exceptions, APIC timer, PS/2 keyboard IRQ
    ├── PMM            — 4 KiB frame bitmap (64 GiB range, 2 MiB bitmap)
    ├── VMM            — 4-level paging, 2 MiB huge pages, direct physical map
    ├── Slab           — 9 size classes (16 B – 4 KiB), kmalloc/kfree
    ├── Drivers
    │     ├── GOP Framebuffer — double-buffered text console (font8x8)
    │     ├── PS/2 Keyboard  — AZERTY-FR, IRQ 1, ring buffer
    │     ├── PCI            — config space enumeration, BAR read, bus mastering
    │     ├── Block          — 32 MiB RAM disk (512-byte sectors)
    │     ├── e1000          — Intel 82540EM NIC (QEMU), DMA TX/RX rings
    │     └── USB
    │           ├── xHCI    — host controller (command/event/transfer rings)
    │           ├── Enum    — CDC-ECM device enumeration
    │           └── RTL8153 — Realtek USB 3.0 Gigabit Ethernet (ThinkPad adapter)
    ├── Scheduler      — preemptive round-robin, APIC ~100 Hz, ASM context switch
    ├── Syscall        — syscall/sysret via STAR/LSTAR/FMASK, ring-3 via iretq
    ├── VFS
    │     ├── Abstraction — Filesystem trait, mount table, path resolution
    │     ├── KernFS      — custom RAM FS (extents, flat inodes, no journal)
    │     └── Path utils  — normalize, basename, dirname, join (zero-alloc)
    ├── Net
    │     ├── Ethernet  — frame dispatch (ARP / IPv4)
    │     ├── ARP       — 16-entry cache, request/reply
    │     ├── IPv4      — checksum RFC 1071, routing, dispatch
    │     ├── ICMP      — echo reply (automatic ping response)
    │     ├── UDP       — 8 sockets, bind/send/recv
    │     └── TCP       — full state machine, connect/send/recv/close, MSS 1440
    └── Shell
          ├── Readline  — echo, backspace, Ctrl-C, Ctrl-L, ESC sequences
          ├── Parser    — tokenizer with single/double quote support
          └── Builtins  — 17 commands (ls, cat, mkdir, rm, mv, stat, write,
                          ping, ps, memstat, netstat, uptime, reboot, halt, …)
```

---

## Tech Stack

| Layer              | Language / Crate                         | Notes                                      |
|--------------------|------------------------------------------|--------------------------------------------|
| Bootloader         | Rust `no_std` + `uefi 0.33` + `xmas-elf 0.10` | Target: `x86_64-unknown-uefi`        |
| Kernel core        | Rust `no_std` + `x86_64 0.15`            | Target: `x86_64-unknown-none`              |
| Framebuffer font   | `font8x8 0.3.1`                          | Embedded bitmap font, no dynamic alloc     |
| Shared types       | Rust `no_std`                            | `BootInfo`, `MemoryMap`, `FramebufferInfo` |
| Build toolchain    | `cargo +nightly`, `build-std`            | `core` + `compiler_builtins` + `alloc`     |
| CPU stubs          | `global_asm!` (x86_64 AT&T)             | Context switch, syscall entry (~50 lines)  |
| Test environment   | QEMU + OVMF                              | `-serial stdio -netdev user,model=e1000`   |
| Real hardware      | Dell Latitude 7340                       | PXE boot, RTL8153 USB NIC, GOP display     |

---

## Project Structure

```
KernOS/
├── .cargo/
│   └── config.toml              # Workspace Cargo config
├── .github/
│   └── workflows/               # CI: build + fmt + clippy
├── bootloader/
│   ├── src/main.rs              # UEFI entry, ELF loading, PXE, BootInfo
│   └── Cargo.toml
├── kernel/
│   ├── src/
│   │   ├── main.rs              # kernel_main — init sequence
│   │   ├── serial.rs            # UART 16550 driver + kprint!/kprintln!
│   │   ├── gdt.rs               # GDT + TSS (kernel + user segments)
│   │   ├── idt.rs               # IDT (32 exceptions + APIC + keyboard)
│   │   ├── pmm.rs               # Physical memory manager (bitmap)
│   │   ├── vmm.rs               # Virtual memory manager (4-level paging)
│   │   ├── slab.rs              # Slab allocator — kmalloc/kfree
│   │   ├── apic.rs              # Local APIC timer (~100 Hz)
│   │   ├── scheduler.rs         # Preemptive round-robin scheduler
│   │   ├── syscall.rs           # syscall/sysret + fd table (musl-compatible)
│   │   ├── drivers/
│   │   │   ├── mod.rs
│   │   │   ├── framebuffer.rs   # GOP framebuffer + font8x8 text console
│   │   │   ├── keyboard.rs      # PS/2 AZERTY-FR, IRQ 1, ring buffer
│   │   │   ├── pci.rs           # PCI bus enumeration + BAR read
│   │   │   ├── block.rs         # 32 MiB RAM disk block device
│   │   │   ├── e1000.rs         # Intel e1000 NIC driver (QEMU)
│   │   │   └── usb/
│   │   │       ├── mod.rs       # USB device enumeration (CDC-ECM)
│   │   │       ├── xhci.rs      # xHCI host controller driver
│   │   │       ├── rtl8153.rs   # Realtek RTL8153 USB Ethernet driver
│   │   │       └── descriptors.rs # USB standard descriptor structs
│   │   ├── vfs/
│   │   │   ├── mod.rs           # VFS abstraction + mount table
│   │   │   ├── kernfs.rs        # KernFS — custom RAM filesystem
│   │   │   └── path.rs          # Path utilities (zero-alloc)
│   │   ├── net/
│   │   │   ├── mod.rs           # Net init + poll loop
│   │   │   ├── ethernet.rs      # Ethernet II framing + dispatch
│   │   │   ├── arp.rs           # ARP cache + request/reply
│   │   │   ├── ip.rs            # IPv4 + Internet checksum
│   │   │   ├── icmp.rs          # ICMP echo reply
│   │   │   ├── udp.rs           # UDP socket API
│   │   │   └── tcp.rs           # TCP state machine
│   │   └── shell/
│   │       ├── mod.rs           # Shell main loop + readline
│   │       ├── parser.rs        # Command tokenizer
│   │       └── builtins.rs      # 17 built-in commands
│   ├── kernel.ld                # Linker script — kernel @ 1 MiB physical
│   └── Cargo.toml
├── shared/
│   └── src/lib.rs               # BootInfo, MemoryMap, FramebufferInfo
├── docs/                        # Design notes and architecture docs
├── CONTRIBUTING.md              # Workflow, branch strategy, commit convention
├── Cargo.toml                   # Workspace root
├── Makefile                     # Build system
└── LICENSE                      # GPL-3.0
```

---

## Prerequisites

```bash
# System packages (Debian/Ubuntu)
sudo apt install -y qemu-system-x86 ovmf mtools gdb dnsmasq tftpd-hpa

# Rust — always via rustup, never via apt
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Nightly toolchain + targets
rustup install nightly
rustup target add x86_64-unknown-uefi --toolchain nightly
rustup target add x86_64-unknown-none --toolchain nightly
rustup component add rust-src --toolchain nightly
```

---

## Build & Run

```bash
# Build bootloader + kernel
make build

# Run in QEMU with serial output on stdout
make run

# Run in QEMU with network (e1000, user-net)
qemu-system-x86_64 \
  -bios /usr/share/ovmf/OVMF.fd \
  -drive format=raw,file=disk.img \
  -netdev user,id=net0,hostfwd=tcp::8080-:80 \
  -device e1000,netdev=net0 \
  -serial stdio -nographic

# Update necessary files for PXE boot with latest updated binaries.
make pxe-update

# Clean build artefacts
make clean
```

To exit QEMU: `Ctrl+A` then `X`.

---

## PXE Boot (Real Hardware)

KernOS supports PXE boot over the network — the bootloader fetches `kernel.elf`
from a TFTP server (default: `192.168.100.1`).

```bash
# On the dev machine — serve the kernel via TFTP
sudo cp target/kernel.elf /var/lib/tftpboot/
sudo systemctl restart tftpd-hpa

# dnsmasq.conf (minimal PXE setup)
# dhcp-range=192.168.100.100,192.168.100.200,12h
# dhcp-boot=bootx64.efi
# enable-tftp
# tftp-root=/var/lib/tftpboot
```

Tested successfully on **Dell Latitude 7340** booting via PXE with graphical
output on the built-in display (GOP framebuffer) and a Lenovo ThinkPad
RTL8153 USB Ethernet adapter for network connectivity.

---

## Shell Commands

Once booted, the interactive shell accepts the following commands:

| Command                    | Description                                      |
|----------------------------|--------------------------------------------------|
| `help`                     | List all available commands                      |
| `clear`                    | Clear the screen (ANSI escape)                   |
| `echo [text...]`           | Print text to the console                        |
| `ls [path]`                | List directory contents                          |
| `cat <file>`               | Display file contents                            |
| `mkdir <path>`             | Create a directory                               |
| `rm <path>`                | Remove a file or empty directory                 |
| `mv <src> <dst>`           | Rename or move a file                            |
| `stat <path>`              | Show file metadata (size, inode, mode)           |
| `write <file> <text>`      | Append text to a file (creates if not found)     |
| `ping <ip>`                | Send 4 ICMP Echo Requests                        |
| `ps`                       | List scheduler tasks with state and TID          |
| `memstat`                  | Physical memory usage (PMM)                      |
| `netstat`                  | Network config (IP, MAC, gateway, ARP cache)     |
| `uptime`                   | Ticks since boot → h/m/s                         |
| `reboot`                   | Reboot via PS/2 controller reset                 |
| `halt`                     | Power off via ACPI (QEMU port 0x604)             |

---

## KernFS

KernFS is a custom RAM filesystem designed specifically for KernOS.
It replaces ext2 for performance reasons:

| Feature            | ext2                      | KernFS                         |
|--------------------|---------------------------|--------------------------------|
| Block size         | 1 KiB (default)           | 4 KiB (aligned to PMM frames)  |
| Metadata location  | On-disk, scattered        | Flat in-memory inode table     |
| Lookup complexity  | O(1) inode + disk I/O     | O(1) pure RAM                  |
| Allocation         | Block bitmap + indirect   | Extent-based, contiguous       |
| Journaling         | No (ext2)                 | None (volatile by design)      |
| Max inodes         | Configured at mkfs        | 4096 (configurable)            |
| Max file size      | ~16 GiB                   | 8 extents × up to 512 blocks   |

Initial directory tree created at boot: `/bin`, `/etc`, `/tmp`, `/dev`, `/proc`, `/home`.

---

## Roadmap

- [x] Bootloader — UEFI, ELF loading, PXE, GOP framebuffer passthrough
- [x] Brick 1 — BSS zeroing, UART 16550 serial, `kprint!`/`kprintln!`
- [x] Brick 2 — GDT (kernel + user segments), IDT (32 exceptions), 8259 PIC, APIC
- [x] Brick 3 — PMM (4 KiB bitmap, huge pages, contiguous alloc, bootloader reclaim)
- [x] Brick 4 — VMM (4-level paging, 2 MiB huge pages, direct map), slab allocator
- [x] Brick 5 — Preemptive scheduler (APIC ~100 Hz, ASM context switch, idle task)
- [x] Brick 6 — Syscall/sysret (STAR/LSTAR/FMASK), ring-3 via iretq, musl-compatible FD table
- [x] Brick 7 — PS/2 keyboard (AZERTY-FR), PCI enumeration, 32 MiB RAM disk, e1000 NIC
- [x] Brick 8 — KernFS (custom RAM FS), VFS abstraction, path utilities
- [x] Brick 9 — TCP/IP stack (Ethernet/ARP/IPv4/ICMP/UDP/TCP), e1000 driver
- [x] Brick 10 — Interactive shell (readline, 17 commands, AZERTY-FR)
- [x] Brick 11 — GOP framebuffer (double-buffered, font8x8, screen mirror of serial)
- [ ] KernOS v2.0.0 with first a refactor of all codes and comments as well as a fix for everything that has to be fixed.
---

## Design Principles

**Aggressive RAM usage** — KernOS is designed to *use* RAM rather than sit on it:
- 2 MiB huge pages reduce TLB pressure on hot paths
- 32 MiB RAM disk allocated contiguously at boot (no fragmentation)
- Direct physical map at `0xFFFF_8800_0000_0000` — any physical frame is accessible with zero pointer arithmetic
- Slab allocator with instant reuse (intrusive free list, zero overhead)

**No runtime surprises** — every piece of hardware the kernel touches is explicitly initialised in `kernel_main` in a deterministic order. No lazy initialisation, no global constructors, no RAII destructors.

**Comments for humans** — every file is documented as if explaining to someone encountering OS concepts for the first time. Every `unsafe` block has a justification comment.

---

## CI

Every push and pull request triggers three jobs:

| Job       | Command                                    |
|-----------|--------------------------------------------|
| `build`   | `cargo +nightly build`                     |
| `fmt`     | `cargo +nightly fmt -- --check`            |
| `clippy`  | `cargo +nightly clippy -- -D warnings`     |

A PR cannot be merged if any job fails. See [CONTRIBUTING.md](CONTRIBUTING.md) for the full workflow, branch naming convention, and commit format.

---

## References (Used by AI mostly at this moment)

- [OSDev Wiki](https://wiki.osdev.org) — primary OS development reference
- [Intel 64 and IA-32 Architectures SDM](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html) — xHCI, APIC, MSRs, paging
- [xHCI Specification](https://www.intel.com/content/www/us/en/products/docs/io/universal-serial-bus/extensible-host-controler-interface-usb-xhci.html) — USB 3.x host controller
- [UEFI Specification](https://uefi.org/specifications) — bootloader and GOP
- [RFC 791](https://datatracker.ietf.org/doc/html/rfc791) — IPv4
- [RFC 793](https://datatracker.ietf.org/doc/html/rfc793) — TCP
- [uefi-rs](https://docs.rs/uefi) — UEFI Rust crate
- [x86_64 crate](https://docs.rs/x86_64) — CPU structures (GDT, IDT, page tables)
- [Redox OS](https://www.redox-os.org) — Rust OS inspiration
- [Linux kernel source](https://github.com/torvalds/linux) — RTL8153 driver reference (`r8152.c`)

---

## License

GPL-3.0 — see [LICENSE](LICENSE) for details.

Anyone can use, study, and modify KernOS, but any derivative work must
remain open-source under the same license.

---

## AI Use

Development is assisted by Claude (Anthropic) and Gemini (Goolgle). AI is used for code generation, inline comments, and documentation — always reviewed and tested by the author.
The codebase is the result of an active learning process: architecture decisions, bug fixes, and hardware debugging are done collaboratively.

If you find a bug or a possible improvement, feel free to open an issue or propose a PR — contributions are welcome.

---

## Status Note

This project is under active development. Architecture, APIs, and the tech stack are subject to change as new bricks are implemented and hardware is tested.
The `main` branch always represents a state that compiles and boots successfully in QEMU. Real-hardware stability is tested on a Dell Latitude 7340.