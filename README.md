# KernOS

> A security-oriented, bare-metal operating system built from scratch in Rust.

KernOS is designed to be lightweight, fast-booting, and security-focused, with a long-term goal of
providing built-in network and cybersecurity tooling at the OS level.

This project is a learning-driven OS development effort, built brick by brick with full documentation
so that anyone — even without prior OS development experience — can follow and understand the codebase.

---

## Status

| Component        | Status      |
|------------------|-------------|
| Bootloader       | Complete    |
| Kernel stub      | Complete    |
| IDT / GDT        | Planned     |
| Memory (PMM/VMM) | Planned     |
| Scheduler        | Planned     |
| Network stack    | Planned     |

---

## Architecture

```
UEFI Firmware
    └── Bootloader (Rust)
            └── Loads kernel.elf from FAT32 partition
            └── Parses ELF segments into memory
            └── Retrieves memory map
            └── Exits UEFI boot services
            └── Jumps to kernel_main()
                    └── Kernel (Rust + minimal ASM)
```

---

## Tech Stack

| Layer              | Language             | Reason                                          |
|--------------------|----------------------|-------------------------------------------------|
| Bootloader         | Rust                 | 100% Rust via uefi crate, no ASM needed         |
| Kernel core        | Rust                 | Memory safety, no_std ecosystem                 |
| CPU stubs          | Rust + minimal ASM   | ~20 lines for interrupt stubs and context switch|
| Network stack      | Rust (+ SPARK Ada ?) | Memory safety critical for packet parsing       |
| Crypto modules     | Rust (+ SPARK Ada ?) | Formal verification for cryptographic proofs    |

---

## Project Structure

```
KernOS/
├── .cargo/
│   └── config.toml          # Workspace-level Cargo configuration
├── bootloader/
│   ├── .cargo/
│   │   └── config.toml      # Bootloader-specific build config (target: x86_64-unknown-uefi)
│   ├── src/
│   │   └── main.rs          # Bootloader entry point and boot sequence
│   └── Cargo.toml
├── kernel/
│   ├── .cargo/
│   │   └── config.toml      # Kernel-specific build config (target: x86_64-unknown-none)
│   ├── src/
│   │   └── main.rs          # Kernel entry point (kernel_main)
│   ├── kernel.ld            # Linker script — places kernel at 1 MB physical
│   └── Cargo.toml
├── .gitignore
├── Cargo.toml               # Workspace root
├── Makefile                 # Build system
└── README.md
```

---

## Prerequisites

Make sure the following tools are installed on your development machine :

```bash
# System packages
sudo apt install -y nasm qemu-system-x86 ovmf mtools gdb

# Rust (always install via rustup, never via apt)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Rust targets
rustup install nightly
rustup target add x86_64-unknown-uefi
rustup target add x86_64-unknown-none
rustup component add rust-src --toolchain nightly
```

---

## Build & Run

```bash
# Compile bootloader and kernel
make build

# Compile and launch in QEMU (recommended for development)
make run

# Delete all compiled output
make clean
```

To exit QEMU : `Ctrl+A` then `X`.

---

## Boot Sequence

1. UEFI firmware loads bootloader.efi from EFI/BOOT/BOOTX64.EFI
2. Bootloader initializes UEFI services
3. Bootloader reads kernel.elf from the FAT32 partition root
4. Bootloader parses ELF segments and copies them to physical memory
5. Bootloader retrieves the system memory map
6. Bootloader exits UEFI boot services (point of no return)
7. Bootloader jumps to kernel_main()
8. Kernel takes full control of the machine

---

## Roadmap

- [x] Bootloader — UEFI, ELF loading, memory map
- [x] Kernel stub — bare-metal entry point
- [ ] Brick 1 — Kernel bootstrap (BSS, serial debug output)
- [ ] Brick 2 — IDT / GDT / Interrupts
- [ ] Brick 3 — Physical Memory Manager (PMM)
- [ ] Brick 4 — Virtual Memory Manager (VMM / Paging)
- [ ] Brick 5 — Process Scheduler
- [ ] Brick 6 — Syscall interface
- [ ] Brick 7 — Device drivers (serial, keyboard, disk, NIC)
- [ ] Brick 8 — Virtual File System (VFS)
- [ ] Brick 9 — TCP/IP Network Stack
- [ ] Brick 10 — Shell and userspace

---

## References

- [OSDev Wiki](https://wiki.osdev.org) — OS development reference
- [UEFI Specification](https://uefi.org/specifications) — UEFI standard
- [xmas-elf](https://docs.rs/xmas-elf) — ELF parser used in the bootloader
- [uefi-rs](https://docs.rs/uefi) — UEFI Rust crate
- [Redox OS](https://www.redox-os.org) — Rust OS for inspiration

---

## License

GPL v3 — see [LICENSE](LICENSE) for details.

This means anyone can use, study, and modify KernOS, but any derivative
work must remain open-source under the same license.