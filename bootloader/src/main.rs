// ============================================================
// KernOS Bootloader
// ============================================================
//
// This is the first code that runs when the computer starts.
// It is loaded by the UEFI firmware and is responsible for :
//
//   1. Initializing UEFI services
//   2. Displaying a boot message
//   3. Loading the kernel from disk
//   4. Parsing the kernel ELF file and mapping it into memory
//   5. Retrieving the system memory map
//   6. Exiting UEFI boot services
//   7. Jumping to the kernel entry point
//
// Language  : Rust (no_std, no_main)
// Target    : x86_64-unknown-uefi
// Author    : Matéo Reymond (AI assisted)
// ============================================================

// We disable the Rust standard library.
// The standard library assumes an OS exists underneath.
// Since we ARE the OS, nothing exists beneath us yet.
#![no_std]
// We disable the automatic main() entry point.
// UEFI has its own entry point convention, handled by the #[entry] macro below.
#![no_main]

// ------------------------------------------------------------
// Imports
// ------------------------------------------------------------

// Core UEFI types : Handle, Status, SystemTable, Boot.
// These are the fundamental building blocks of any UEFI application.
use uefi::prelude::*;

// MemoryType classifies memory zones.
// Example : LOADER_DATA = memory used by the bootloader.
use uefi::boot::MemoryType;

// File system types for reading files from the FAT32 partition.
// SimpleFileSystem = the UEFI protocol to access a FAT32 volume.
// File             = base trait for file and directory operations.
// FileMode         = read, write, or create.
// FileAttribute    = file attributes (hidden, read-only, etc.).
// FileType         = regular file or directory.
// FileInfo         = file metadata (name, size, timestamps).
use uefi::proto::media::file::{File, FileAttribute, FileInfo, FileMode, FileType};
use uefi::proto::media::fs::SimpleFileSystem;

// ELF parser types.
// ElfFile    = the parsed representation of an ELF binary.
// Type       = segment type (LOAD, DYNAMIC, NOTE, etc.).
// SegmentData = the raw data contained in an ELF segment.
use xmas_elf::program::{SegmentData, Type};
use xmas_elf::ElfFile;

// ------------------------------------------------------------
// Entry Point
// ------------------------------------------------------------

// #[entry] marks this function as the UEFI entry point.
// The firmware calls this function after loading our .efi file.
// It replaces the classic fn main() used in normal Rust programs.
#[entry]
fn main() -> Status {
    init_uefi();
    display_banner();

    let kernel_data = load_kernel();
    let entry_point = parse_and_load_elf(kernel_data);

    get_memory_map();
    exit_boot_services();
    jump_to_kernel(entry_point);
}

// ------------------------------------------------------------
// Step 1 : Initialize UEFI services
// ------------------------------------------------------------

/// Initializes the UEFI helper library.
///
/// This must be the very first call in the bootloader.
/// Without it, no UEFI service (screen, disk, memory) is available.
/// Calling any UEFI function before this will result in a crash.
fn init_uefi() {
    uefi::helpers::init().unwrap();
}

// ------------------------------------------------------------
// Step 2 : Display boot banner
// ------------------------------------------------------------

/// Clears the screen and displays the KernOS boot banner.
///
/// Uses the UEFI text output protocol (stdout).
/// Note : UEFI requires UTF-16 strings, hence the cstr16! macro.
/// Note : UEFI requires \r\n (carriage return + line feed), not just \n.
fn display_banner() {
    uefi::system::with_stdout(|stdout| {
        stdout.clear().unwrap();
        stdout
            .output_string(cstr16!("============================================\r\n"))
            .unwrap();
        stdout
            .output_string(cstr16!("  KernOS v1.2.0\r\n"))
            .unwrap();
        stdout
            .output_string(cstr16!("  Bootloader starting...\r\n"))
            .unwrap();
        stdout
            .output_string(cstr16!("============================================\r\n"))
            .unwrap();
    });
}

// ------------------------------------------------------------
// Step 3 : Load the kernel from disk
// ------------------------------------------------------------

/// # First version explaination here :
/// Reads the kernel ELF file from the FAT32 partition into memory.
///
/// The kernel file must be located at the root of the EFI partition
/// and named exactly "kernel.elf". This path is fixed by convention.
///
/// # How it works
/// 1. Locate the UEFI SimpleFileSystem protocol (FAT32 access).
/// 2. Open the root directory of the partition.
/// 3. Open "kernel.elf" in read-only mode.
/// 4. Read the file size from its metadata.
/// 5. Allocate a memory buffer large enough to hold the file.
/// 6. Read the entire file into that buffer.
///
/// # Returns
/// A mutable byte slice containing the raw ELF file data.
/// This slice is valid until we exit UEFI boot services.
///
/// # New Version explaination here :
/// Loads the kernel into memory.
///
/// Tries two methods in order :
/// 1. SimpleFileSystem — works in QEMU and when a FAT32 disk is present.
/// 2. PXE TFTP download — works when booted over the network.
///
/// # Returns
/// A mutable byte slice containing the raw ELF file data.
fn load_kernel() -> &'static mut [u8] {
    // First attempt : try to load from a FAT32 filesystem.
    // This works in QEMU (disk image) and on real hardware with a FAT32 partition.
    if let Some(data) = try_load_from_disk() {
        return data;
    }

    // Second attempt : try to download via PXE TFTP.
    // This works when the machine booted over the network.
    if let Some(data) = try_load_via_pxe() {
        return data;
    }

    // Both methods failed — we cannot continue.
    panic!("Failed to load kernel.elf — no disk and no PXE available");
}

/// Attempts to load kernel.elf from a FAT32 filesystem.
/// Returns None if no filesystem is available or the file is not found.
fn try_load_from_disk() -> Option<&'static mut [u8]> {
    // Try to find a SimpleFileSystem handle.
    // This fails if there is no FAT32 partition accessible.
    let fs_handle = uefi::boot::get_handle_for_protocol::<SimpleFileSystem>().ok()?;

    let mut fs = uefi::boot::open_protocol_exclusive::<SimpleFileSystem>(fs_handle).ok()?;
    let mut root = fs.open_volume().ok()?;

    let kernel_handle = root
        .open(
            cstr16!("kernel.elf"),
            FileMode::Read,
            FileAttribute::empty(),
        )
        .ok()?;

    let mut kernel_file = match kernel_handle.into_type().ok()? {
        FileType::Regular(f) => f,
        _ => return None,
    };

    let mut info_buffer = [0u8; 256];
    let info = kernel_file.get_info::<FileInfo>(&mut info_buffer).ok()?;
    let kernel_size = info.file_size() as usize;

    let kernel_buffer = uefi::boot::allocate_pool(MemoryType::LOADER_DATA, kernel_size).ok()?;

    let kernel_data =
        unsafe { core::slice::from_raw_parts_mut(kernel_buffer.as_ptr(), kernel_size) };

    kernel_file.read(kernel_data).ok()?;

    uefi::system::with_stdout(|stdout| {
        stdout
            .output_string(cstr16!("[OK] Kernel loaded from disk\r\n"))
            .unwrap();
    });

    Some(kernel_data)
}

/// Attempts to download kernel.elf from the PXE TFTP server.
/// Returns None if PXE is not available.
fn try_load_via_pxe() -> Option<&'static mut [u8]> {
    use uefi::proto::network::pxe::BaseCode;
    use uefi::proto::network::IpAddress;

    let pxe_handle = uefi::boot::get_handle_for_protocol::<BaseCode>().ok()?;
    let mut pxe = uefi::boot::open_protocol_exclusive::<BaseCode>(pxe_handle).ok()?;

    // Hardcode the TFTP server IP.
    // In our lab setup, the PXE server is always the VM at 192.168.100.1.
    // This avoids having to parse the private DHCP packet fields.
    let server_ip = IpAddress::new_v4([192, 168, 100, 1]);

    let file_size: u64 = match pxe.tftp_get_file_size(&server_ip, cstr8!("kernel.elf")) {
        Ok(size) => size,
        Err(_) => return None,
    };
    let kernel_size = file_size as usize;

    let kernel_buffer = uefi::boot::allocate_pool(MemoryType::LOADER_DATA, kernel_size).ok()?;

    let kernel_data =
        unsafe { core::slice::from_raw_parts_mut(kernel_buffer.as_ptr(), kernel_size) };

    pxe.tftp_read_file(&server_ip, cstr8!("kernel.elf"), Some(kernel_data))
        .ok()?;

    uefi::system::with_stdout(|stdout| {
        stdout
            .output_string(cstr16!("[OK] Kernel loaded via PXE TFTP\r\n"))
            .unwrap();
    });

    Some(kernel_data)
}

// ------------------------------------------------------------
// Step 4 : Parse the ELF file and load segments into memory
// ------------------------------------------------------------

/// Parses the kernel ELF binary and copies each loadable segment
/// into its correct physical memory address.
///
/// # What is an ELF file ?
/// ELF (Executable and Linkable Format) is the standard binary format
/// for executables on Linux and bare-metal x86_64 systems.
/// It contains :
///   - A header  : architecture, entry point address, etc.
///   - Segments  : blocks of code or data to load into memory.
///   - Sections  : debug info, symbol tables (not needed here).
///
/// # What we do here
/// 1. Parse the ELF header to validate the binary.
/// 2. Iterate over all program headers (segment descriptors).
/// 3. For each LOAD segment, copy its data to the correct RAM address.
/// 4. Extract and return the kernel entry point address.
///
/// # Why only LOAD segments ?
/// Only LOAD segments contain code or data that must be in RAM
/// for the kernel to execute. Other types (NOTE, DYNAMIC, GNU_STACK)
/// are metadata for tools and are not needed at runtime.
///
/// # Returns
/// A raw function pointer to the kernel entry point.
/// The bootloader will jump to this address in Step 7.
fn parse_and_load_elf(kernel_data: &[u8]) -> *const () {
    // Parse the raw bytes as an ELF file.
    // ElfFile::new validates the ELF magic number (0x7F 'E' 'L' 'F')
    // and checks that the architecture is x86_64.
    let elf = ElfFile::new(kernel_data).unwrap();

    // Iterate over every program header in the ELF file.
    // Each program header describes one segment.
    for segment in elf.program_iter() {
        // Skip segments that are not of type LOAD.
        // Only LOAD segments need to be copied into RAM.
        if segment.get_type().unwrap() != Type::Load {
            continue;
        }

        // Get the target physical address for this segment.
        // This is where in RAM we must copy the segment data.
        // The ELF linker script determines these addresses.
        let phys_addr = segment.physical_addr() as *mut u8;

        // Get the raw bytes of this segment from the ELF file.
        // SegmentData::Undefined is the variant for raw binary data,
        // which is what code and initialized data segments contain.
        let data = match segment.get_data(&elf).unwrap() {
            SegmentData::Undefined(data) => data,
            _ => continue,
        };

        // Copy the segment bytes to the target RAM address.
        // copy_nonoverlapping is equivalent to memcpy in C.
        // It requires unsafe because we write to a raw pointer.
        // We trust the ELF linker script to have placed segments
        // at valid, non-overlapping memory addresses.
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), phys_addr, data.len());
        }
    }

    uefi::system::with_stdout(|stdout| {
        stdout
            .output_string(cstr16!("[OK] Kernel ELF segments loaded\r\n"))
            .unwrap();
    });

    // Extract the kernel entry point from the ELF header.
    // This is the virtual address of the first instruction to execute.
    // We cast it to a raw function pointer for use in jump_to_kernel().
    elf.header.pt2.entry_point() as *const ()
}

// ------------------------------------------------------------
// Step 5 : Retrieve the memory map
// ------------------------------------------------------------

/// Asks the UEFI firmware for the current system memory map.
///
/// The memory map is a list of all physical memory zones, each described
/// by a type (free, used by firmware, reserved by hardware, etc.).
///
/// The kernel needs this map to know which memory zones it can use
/// and which ones it must avoid.
///
/// # Timing
/// This must be called as late as possible, right before exiting
/// UEFI boot services. The memory map changes every time UEFI
/// allocates or frees memory, so an early snapshot would be stale.
///
/// After exit_boot_services(), we can no longer call this function.
fn get_memory_map() {
    // Request the memory map from UEFI.
    // MemoryType::LOADER_DATA tells UEFI to allocate the map buffer
    // in a LOADER_DATA zone, so the kernel can find and reuse it.
    // We prefix with _ to silence the unused variable warning.
    // The map is consumed internally by exit_boot_services().
    let _memory_map = uefi::boot::memory_map(MemoryType::LOADER_DATA).unwrap();

    uefi::system::with_stdout(|stdout| {
        stdout
            .output_string(cstr16!("[OK] Memory map retrieved\r\n"))
            .unwrap();
    });
}

// ------------------------------------------------------------
// Step 6 : Exit UEFI boot services
// ------------------------------------------------------------

/// Transfers full control of the machine from UEFI to our kernel.
///
/// This is the point of no return. After this function :
///   - All UEFI services are gone (no screen, no disk, no memory services).
///   - The UEFI firmware frees its own memory zones.
///   - The CPU belongs entirely to the kernel.
///
/// # Why is this unsafe ?
/// exit_boot_services() causes the firmware to free its own memory.
/// Pointers that were valid one moment before may become invalid instantly.
/// The Rust compiler cannot verify this, so we must use unsafe
/// and take full responsibility for correctness.
fn exit_boot_services() {
    uefi::system::with_stdout(|stdout| {
        stdout
            .output_string(cstr16!("Exiting UEFI boot services...\r\n"))
            .unwrap();
    });

    // Exit UEFI boot services.
    // After this line, no UEFI call is valid anymore.
    // MemoryType::LOADER_DATA tells UEFI where to store the final
    // memory map that describes what it freed when shutting down.
    let _final_memory_map = unsafe { uefi::boot::exit_boot_services(MemoryType::LOADER_DATA) };
}

// ------------------------------------------------------------
// Step 7 : Jump to the kernel entry point
// ------------------------------------------------------------

/// Transfers execution to the kernel by calling its entry point.
///
/// This function never returns. Once the kernel starts, the bootloader
/// is gone. The kernel takes full and permanent control of the machine.
///
/// # How it works
/// The entry point is a raw memory address obtained from the ELF header.
/// We cast it to a Rust function pointer and call it.
/// The "C" ABI ensures the call follows the standard x86_64 calling
/// convention that the kernel expects.
///
/// # Why is this unsafe ?
/// We are calling a raw function pointer. The compiler has no way
/// to verify that this address is valid or that the function signature
/// matches. We trust the ELF parser to have given us the correct address.
///
/// # Why -> ! ?
/// The ! return type means "this function never returns".
/// Rust requires this here because main() must return a Status,
/// but after jumping to the kernel, we never come back.
fn jump_to_kernel(entry_point: *const ()) -> ! {
    unsafe {
        // Cast the raw address to a callable function pointer.
        // extern "C" = use the standard C calling convention (ABI).
        // fn() -> !  = a function that takes no arguments and never returns.
        let kernel_entry: extern "C" fn() -> ! = core::mem::transmute(entry_point);

        // Call the kernel entry point.
        // This transfers control permanently to the kernel.
        // The bootloader never executes another instruction after this.
        kernel_entry()
    }
}

// ------------------------------------------------------------
// Panic Handler
// ------------------------------------------------------------

/// Called automatically by Rust if the program panics.
///
/// A panic occurs when an unwrap() fails or when the program
/// reaches an explicitly unreachable state.
///
/// In no_std mode, Rust requires us to define this handler ourselves
/// because the standard library's panic handler is not available.
///
/// Currently we loop forever. A future improvement would be to
/// display the panic message on screen before halting.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    uefi::system::with_stdout(|stdout| {
        stdout.output_string(cstr16!("Problem...\r\n")).unwrap();
    });
    loop {}
}
