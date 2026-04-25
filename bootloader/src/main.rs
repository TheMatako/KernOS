// ============================================================
// KernOS Bootloader
// ============================================================
//
// This is the first code that runs when the computer starts.
// It is loaded by the UEFI firmware and is responsible for :
//
//   1. Initializing UEFI services
//   2. Displaying a boot banner
//   3. Loading the kernel from disk or via PXE TFTP
//   4. Parsing the kernel ELF and mapping segments into memory
//   5. Collecting framebuffer and ACPI information
//   6. Retrieving the UEFI memory map
//   7. Exiting UEFI boot services
//   8. Converting the UEFI memory map into our BootInfo format
//   9. Jumping to kernel_main() with a pointer to BootInfo
//
// Language : Rust (no_std, no_main)
// Target   : x86_64-unknown-uefi
// Author   : Matéo Reymond (AI assisted)
// ============================================================

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
// ------------------------------------------------------------
// Imports
// ------------------------------------------------------------

use shared::{BootInfo, FramebufferInfo, MemoryRegion, MemoryRegionKind};
use uefi::boot::MemoryType;
use uefi::mem::memory_map::MemoryMap as UefiMemoryMap;
use uefi::prelude::*;
use uefi::proto::media::file::{File, FileAttribute, FileInfo, FileMode, FileType};
use uefi::proto::media::fs::SimpleFileSystem;
use xmas_elf::program::{SegmentData, Type};
use xmas_elf::ElfFile;

// ------------------------------------------------------------
// Entry point
// ------------------------------------------------------------

static mut BOOT_INFO_STORAGE: BootInfo = BootInfo::new();

#[entry]
fn main() -> Status {
    init_uefi();
    display_banner();

    // Create the BootInfo structure that we will pass to the kernel.
    // It starts empty and gets filled step by step below.
    let boot_info = unsafe { &mut BOOT_INFO_STORAGE };

    // Load the kernel binary into memory from disk or via PXE.
    let kernel_data = load_kernel();

    // Parse the ELF file, copy segments into RAM, and record
    // the kernel's physical location in boot_info.
    let entry_point = parse_and_load_elf(kernel_data, boot_info);

    // Collect GOP framebuffer address and dimensions.
    fill_framebuffer_info(boot_info);

    // Collect the ACPI RSDP address for later hardware initialization.
    fill_rsdp_info(boot_info);

    // Get the UEFI memory map right before exiting boot services.
    // We let Rust infer the concrete type — it is not publicly exposed.
    // This must be done as late as possible since the map changes
    // every time UEFI allocates or frees memory.
    let uefi_memory_map = uefi::boot::memory_map(MemoryType::LOADER_DATA).unwrap();

    uefi::system::with_stdout(|stdout| {
        stdout
            .output_string(cstr16!("[OK] Memory map retrieved\r\n"))
            .unwrap();
        stdout
            .output_string(cstr16!("Exiting UEFI boot services...\r\n"))
            .unwrap();
    });

    // Point of no return — all UEFI services are gone after this line.
    // No more screen output, no more disk access, no more memory services.
    // The CPU belongs entirely to us.
    unsafe {
        let _ = uefi::boot::exit_boot_services(MemoryType::LOADER_DATA);
    };

    // Convert the UEFI memory map into our own BootInfo format.
    // We do this AFTER exit_boot_services() because the map is now
    // stable — no more allocations or frees will change it.
    //
    // entries() iterates over every memory descriptor.
    // Each descriptor describes one contiguous zone of physical RAM.
    for descriptor in uefi_memory_map.entries() {
        // Convert the UEFI memory type into our MemoryRegionKind enum.
        // This hides UEFI internals from the kernel — it only sees our types.
        // match works like switch/case : it compares descriptor.ty to each arm.
        let kind = match descriptor.ty {
            // CONVENTIONAL = normal free RAM the kernel can use freely.
            MemoryType::CONVENTIONAL => MemoryRegionKind::Usable,

            // LOADER_CODE and LOADER_DATA = our bootloader's own memory.
            // The "|" means "either of these two values matches".
            // The kernel can reclaim this memory once boot is complete.
            MemoryType::LOADER_CODE | MemoryType::LOADER_DATA => {
                MemoryRegionKind::BootloaderReclaimable
            }

            // RUNTIME_SERVICES = UEFI runtime memory that must be preserved.
            // Even after exit_boot_services(), some UEFI calls still need it.
            MemoryType::RUNTIME_SERVICES_CODE | MemoryType::RUNTIME_SERVICES_DATA => {
                MemoryRegionKind::UefiRuntime
            }

            // Everything else (ACPI, MMIO, unknown) is reserved.
            // The underscore "_" is a wildcard matching anything not listed above.
            _ => MemoryRegionKind::Reserved,
        };

        // Build a MemoryRegion from the UEFI descriptor and add it to our map.
        // phys_start = physical base address of this zone.
        // page_count = size in 4KB pages — multiply by 4096 to get bytes.
        boot_info.memory_map.add_entry(MemoryRegion {
            base: descriptor.phys_start,
            length: descriptor.page_count * 4096,
            kind,
        });
    }

    // Jump to the kernel, passing a pointer to our completed BootInfo.
    jump_to_kernel(entry_point, boot_info);
}
// ------------------------------------------------------------
// Step 1 : Initialize UEFI services
// ------------------------------------------------------------

/// Initializes the UEFI helper library.
/// Must be the very first call — nothing works before this.
fn init_uefi() {
    uefi::helpers::init().unwrap();
}

// ------------------------------------------------------------
// Step 2 : Display boot banner
// ------------------------------------------------------------

/// Clears the screen and displays the KernOS boot banner.
fn display_banner() {
    uefi::system::with_stdout(|stdout| {
        stdout.clear().unwrap();
        stdout
            .output_string(cstr16!("============================================\r\n"))
            .unwrap();
        stdout
            .output_string(cstr16!("  KernOS v1.0.0\r\n"))
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
// Step 3 : Load the kernel
// ------------------------------------------------------------

/// Loads the kernel into memory.
///
/// Tries two methods in order :
/// 1. SimpleFileSystem — works in QEMU and with a local FAT32 disk.
/// 2. PXE TFTP — works when booted over the network.
fn load_kernel() -> &'static mut [u8] {
    if let Some(data) = try_load_from_disk() {
        return data;
    }
    if let Some(data) = try_load_via_pxe() {
        return data;
    }
    panic!("Failed to load kernel.elf — no disk and no PXE available");
}

/// Attempts to load kernel.elf from a FAT32 filesystem.
/// Returns None if no filesystem is available or the file is not found.
fn try_load_from_disk() -> Option<&'static mut [u8]> {
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

    // The TFTP server is always our VM at 192.168.100.1 in our lab setup.
    let server_ip = IpAddress::new_v4([192, 168, 100, 1]);

    let file_size: u64 = match pxe.tftp_get_file_size(&server_ip, cstr8!("kernel.elf")) {
        Ok(size) => size,
        Err(_) => return None,
    };
    let kernel_size = file_size as usize;

    let kernel_buffer = uefi::boot::allocate_pool(MemoryType::LOADER_DATA, kernel_size).ok()?;
    let kernel_data =
        unsafe { core::slice::from_raw_parts_mut(kernel_buffer.as_ptr(), kernel_size) };

    match pxe.tftp_read_file(&server_ip, cstr8!("kernel.elf"), Some(kernel_data)) {
        Ok(_) => {}
        Err(_) => return None,
    }

    uefi::system::with_stdout(|stdout| {
        stdout
            .output_string(cstr16!("[OK] Kernel loaded via PXE TFTP\r\n"))
            .unwrap();
    });

    Some(kernel_data)
}

// ------------------------------------------------------------
// Step 4 : Parse ELF and load segments
// ------------------------------------------------------------

/// Parses the kernel ELF binary, copies each LOAD segment into
/// its correct physical address, and records the kernel location
/// in boot_info so the PMM can mark it as non-free.
///
/// Returns the kernel entry point address.
fn parse_and_load_elf(kernel_data: &[u8], boot_info: &mut BootInfo) -> *const () {
    // Parse the raw bytes as an ELF file.
    // ElfFile::new checks the magic number (0x7F 'E' 'L' 'F') at the
    // start of the file to confirm it is a valid ELF binary.
    // It also checks the architecture is x86_64.
    // unwrap() panics if the file is invalid — we cannot continue without a valid kernel.
    let elf = ElfFile::new(kernel_data).unwrap();

    // We track the lowest and highest physical addresses used by the kernel.
    // At the end, we use these to tell the kernel where it lives in RAM
    // so the PMM can mark that zone as "do not overwrite".
    let mut kernel_start = u64::MAX; // start at maximum so any address is lower
    let mut kernel_end = 0u64; // start at zero so any address is higher

    // An ELF file contains multiple "segments" — blocks of code or data.
    // We iterate over every segment header to find the ones we need to load.
    for segment in elf.program_iter() {
        // We only care about segments of type LOAD.
        // LOAD segments contain actual code and data that must be in RAM.
        // Other types (NOTE, GNU_STACK, DYNAMIC) are metadata — skip them.
        if segment.get_type().unwrap() != Type::Load {
            continue; // "continue" skips to the next iteration of the loop
        }

        // physical_addr() tells us where in RAM this segment must be placed.
        // This address comes from our linker script (kernel.ld).
        // For example, 0x100000 = 1 MB — where we told the linker to place the kernel.
        let phys_addr = segment.physical_addr();

        // mem_size() is the size of this segment in memory.
        // It may be larger than the data in the file (e.g. BSS — zero-initialized data).
        let seg_size = segment.mem_size();

        // Update our tracking of the kernel's memory range.
        // After the loop, kernel_start..kernel_end covers the entire kernel.
        if phys_addr < kernel_start {
            kernel_start = phys_addr;
        }
        if phys_addr + seg_size > kernel_end {
            kernel_end = phys_addr + seg_size;
        }

        // Cast the physical address to a raw pointer so we can write to it.
        // *mut u8 means "a pointer to a mutable byte".
        let dst = phys_addr as *mut u8;

        // get_data() extracts the raw bytes of this segment from the ELF file.
        // SegmentData::Undefined is the variant for raw binary data — which is
        // what code (.text) and initialized data (.data) segments contain.
        // If it is any other variant (e.g. Note), we skip it with "continue".
        let data = match segment.get_data(&elf).unwrap() {
            SegmentData::Undefined(data) => data,
            _ => continue,
        };

        // Copy the segment bytes from the ELF file into RAM at the target address.
        // copy_nonoverlapping is the Rust equivalent of memcpy in C.
        // It copies data.len() bytes from data.as_ptr() to dst.
        // unsafe is required because we are writing to a raw pointer —
        // the compiler cannot verify the destination address is valid.
        // We trust our linker script to have chosen a safe address.
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
        }
    }

    // Store the kernel's physical location in boot_info.
    // The PMM (Brick 3) will use this to mark kernel memory as non-free.
    // Without this, the PMM might hand out kernel memory to someone else — catastrophic.
    boot_info.kernel_physical_start = kernel_start;
    boot_info.kernel_size = kernel_end - kernel_start;

    uefi::system::with_stdout(|stdout| {
        stdout
            .output_string(cstr16!("[OK] Kernel ELF segments loaded\r\n"))
            .unwrap();
    });

    // Return the entry point address from the ELF header.
    // This is the address of kernel_main() — the first function the kernel runs.
    // We cast it to *const () which is Rust's way of saying "a raw function pointer
    // whose exact type we don't know yet". jump_to_kernel() will cast it properly.
    elf.header.pt2.entry_point() as *const ()
}
// ------------------------------------------------------------
// Step 5 : Collect framebuffer information
// ------------------------------------------------------------

/// Retrieves the UEFI GOP framebuffer address and dimensions
/// and stores them in boot_info.
///
/// The kernel display driver (Brick 2) uses this to draw pixels.
/// If no GOP framebuffer is available, boot_info.framebuffer stays None.
fn fill_framebuffer_info(boot_info: &mut BootInfo) {
    // GraphicsOutput is the UEFI protocol for screen access (GOP = Graphics Output Protocol).
    // We ask UEFI for a handle (identifier) that supports this protocol.
    // If no graphics output is available, we skip silently — the kernel can still run
    // in text mode or without any display.
    use uefi::proto::console::gop::GraphicsOutput;

    let gop_handle = match uefi::boot::get_handle_for_protocol::<GraphicsOutput>() {
        Ok(h) => h,
        Err(_) => {
            uefi::system::with_stdout(|stdout| {
                stdout
                    .output_string(cstr16!("[WARN] No GOP framebuffer found\r\n"))
                    .unwrap();
            });
            // Return early — boot_info.framebuffer stays None.
            return;
        }
    };

    // Open the GraphicsOutput protocol so we can query it.
    // "exclusive" means no other UEFI agent uses it simultaneously.
    let mut gop = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle).unwrap();

    // current_mode_info() returns the current screen resolution and pixel format.
    let mode = gop.current_mode_info();

    // resolution() returns (width, height) in pixels.
    let (width, height) = mode.resolution();

    // stride() is the number of pixels per row including any alignment padding.
    // It is >= width. The actual bytes per row = stride * bytes_per_pixel.
    let stride = mode.stride() as u32;

    // frame_buffer() gives us direct access to the video memory.
    // as_mut_ptr() returns the physical base address as a raw pointer.
    // Anything written here appears directly on screen.
    let framebuffer_base = gop.frame_buffer().as_mut_ptr() as u64;

    // size() returns the total size of the framebuffer in bytes.
    let framebuffer_size = gop.frame_buffer().size();

    // Fill our FramebufferInfo structure with everything the kernel needs.
    // The Some() wraps the value in an Option — it means "a value is present".
    boot_info.framebuffer = Some(FramebufferInfo {
        base: framebuffer_base,
        size: framebuffer_size,
        width: width as u32,
        height: height as u32,
        stride,
        // UEFI GOP uses 32-bit pixels : 8 bits each for Blue, Green, Red, and padding.
        bytes_per_pixel: 4,
    });

    uefi::system::with_stdout(|stdout| {
        stdout
            .output_string(cstr16!("[OK] Framebuffer info collected\r\n"))
            .unwrap();
    });
}
// ------------------------------------------------------------
// Step 6 : Collect ACPI RSDP address
// ------------------------------------------------------------

/// Finds the ACPI RSDP table address from the UEFI configuration table.
/// The kernel will use this later for advanced hardware initialization.
fn fill_rsdp_info(boot_info: &mut BootInfo) {
    use uefi::table::cfg;

    // The UEFI system table contains a list of configuration tables.
    // We look for the ACPI 2.0 RSDP entry (preferred over ACPI 1.0).
    let rsdp = uefi::system::with_config_table(|tables| {
        // First try ACPI 2.0
        for entry in tables {
            if entry.guid == cfg::ACPI2_GUID {
                return Some(entry.address as u64);
            }
        }
        // Fall back to ACPI 1.0
        for entry in tables {
            if entry.guid == cfg::ACPI_GUID {
                return Some(entry.address as u64);
            }
        }
        None
    });

    boot_info.rsdp_address = rsdp;

    uefi::system::with_stdout(|stdout| {
        if boot_info.rsdp_address.is_some() {
            stdout
                .output_string(cstr16!("[OK] ACPI RSDP found\r\n"))
                .unwrap();
        } else {
            stdout
                .output_string(cstr16!("[WARN] No ACPI RSDP found\r\n"))
                .unwrap();
        }
    });
}

// ------------------------------------------------------------
// Step 7 : Jump to kernel
// ------------------------------------------------------------

/// Transfers execution permanently to the kernel.
///
/// Passes a pointer to BootInfo as the first argument so the
/// kernel has access to all boot-time information.
///
/// The kernel entry point must have this exact signature :
///   pub extern "C" fn kernel_main(boot_info: *const BootInfo) -> !
fn jump_to_kernel(entry_point: *const (), boot_info: &BootInfo) -> ! {
    unsafe {
        // core::mem::transmute reinterprets raw bytes as a different type.
        // Here we cast a raw address (*const ()) into a typed function pointer.
        // The function signature must exactly match kernel_main() in the kernel :
        //   pub extern "C" fn kernel_main(boot_info: *const BootInfo) -> !
        //
        // extern "C" means the function uses the standard C calling convention.
        // On x86_64, the first argument is always passed in the "rdi" register.
        // So boot_info's address will be in rdi when the kernel starts.
        let kernel_entry: extern "sysv64" fn(*const BootInfo) -> ! =
            core::mem::transmute(entry_point);

        // Call the kernel entry point with a pointer to our BootInfo.
        // "as *const BootInfo" converts the reference &BootInfo into a raw pointer.
        // After this line, the bootloader never executes another instruction.
        kernel_entry(boot_info as *const BootInfo)
    }
}
// ------------------------------------------------------------
// Panic handler
// ------------------------------------------------------------

/// Called if any unwrap() fails or the program reaches an
/// unreachable state. Displays an error and loops forever.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    uefi::system::with_stdout(|stdout| {
        stdout
            .output_string(cstr16!("[PANIC] Bootloader panic\r\n"))
            .unwrap();
    });
    loop {}
}
