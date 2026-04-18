// We tell the Rust compiler to not use the standard library.
// The standard library assumes there is an OS underneath.
// Since we are the bootloader, there is nothing underneath us, se we cannot use it.
#![no_std]

// We tell the Rust compiler that we don't have a classic main() function.
// Normally Rust looks for a function called main() to start the program.
// In UEFI, the firmware starts our program its own way, with its own rules.
// So we disable the automatic main() behavior.
#![no_main]

// We import the most commonly used UEFI types and functions.
// "use" is like "import"
// "prelude::*" means everything that is commonly needed.
use uefi::prelude::*;

// We import MemoryType which descibes what a memory zone is used for.
use uefi::boot::MemoryType; 

// We import the UEFI file system types to read files from disk.
// SimpleFileSystem is the UEFI protocol to access FAT32 partition.
// FileMode and FileAttribute define how we open the file (read only, etc.)
use uefi::proto::media::file::{File, FileMode, FileAttribute, FileType, FileInfo};
use uefi::proto::media::fs::SimpleFileSystem;

// We import the ELF parser.
// This lets us read the ELF header and program headers without
// writing the parsing code ourselves.
use xmas_elf::ElfFile;
use xmas_elf::program::{Type, SegmentData};

// This attibute tells the UEFI library "this function is the entry point".
// It replaces the classic fn main() that Rust normally looks for.
// The firmware will call this function when it loads our bootloader.
#[entry]
fn main() -> Status {

    // -----------------------------------------------------
    // STEP 1 : Initialize UEFI services
    // -----------------------------------------------------
    // Before we can use any UEFI feature (screen, memory, disk...),
    // we must initialize the UEFI library first.
    // If we skip this, everything below will crash.
    // unwrap() means "if this fails, panic and stop immediately".
    uefi::helpers::init().unwrap();
    
    // -----------------------------------------------------
    // STEP 2 : Display a message on screen
    // -----------------------------------------------------
    // with_stdout gives us access to the screen for the duration of
    // this block. When the block ends, access is closed cleanly.
    // The parameter between | | is the screen handle given to us
    // by with_stdout. Here we named it "stdout".
    uefi::system::with_stdout(|stdout| {
        // cstr16! converts our text to UTF-16 encoding.
        // UEFI requires UTF-16, it does not understand normal UTF-8 strings.
        // In UEFI we always need both \r and \n.
        stdout.clear().unwrap();
        stdout.output_string(cstr16!("KernOS v0.1.0\r\n")).unwrap();
        stdout.output_string(cstr16!("Bootloader O.K. !\r\n")).unwrap();
    });

    // -----------------------------------------------------
    // STEP 3 : Load the kernel from disk
    // -----------------------------------------------------
    // We need to do this before exiting UEFI boot services.
    // After Step 5, we can no longer access the disk.
    
    // First, we get access to the file system via UEFI.
    // locate_protocol finds the SimpleFileSystem service.
    // This gives us access to the FAT32 partition.
    let fs_handle = uefi::boot::get_handle_for_protocol::<SimpleFileSystem>().unwrap();

    // We open the file system protocol on that handle
    let mut fs = uefi::boot::open_protocol_exclusive::<SimpleFileSystem>(fs_handle).unwrap();

    // We open the root directory of the FAT32 partition
    let mut root = fs.open_volume().unwrap();

    // We open the kernel file in read-only mode.
    // The kernel must be at the root of the partition : \kernel.elf
    // FileMode::Read = open for reading only.
    // FileAttribute::empty() = no special attributes needed.
    let kernel_file_handle = root
        .open(
            cstr16!("kernel.elf"),
            FileMode::Read,
            FileAttribute::empty(),
        ).unwrap();
    
    // We convert the file handle to a RegularFile so we can read it.
    // into_type() checks whether it is a file or a directory.
    // We unwrap and match to get the RegularFile variant.
    let mut kernel_file = match kernel_file_handle.into_type().unwrap() {
        FileType::Regular(f) => f,
        _ => panic!("kernel.elf is not a regular file"),
    };

    // We need to know the file size before reading it.
    // FileInfo contains metadata : name, size, timestamps...
    // We allocate a buffer of 256 bytes to store this info.
    let mut info_buffer = [0u8; 256];
    let info = kernel_file
        .get_info::<FileInfo>(&mut info_buffer)
        .unwrap();

    // We get the file size in bytes.
    let kernel_size = info.file_size() as usize;

    // We allocate a memory zone to store the raw kernel file content.
    // allocate_pool asks UEFI for a contiguous block of memory.
    // MemoryType::LOADER_DATA = this memory is used by the bootloader.
    // kernel_size = we need exactly enough bytes to fit the whole file.
    let kernel_buffer = uefi::boot::allocate_pool(
        MemoryType::LOADER_DATA,
        kernel_size,
    ).unwrap();

    // We create a mutable slice from the raw pointer.
    // A slice is a Rust type that represents a contiguous block of memory
    // with a known length. It lets us work with the buffer safely.
    // unsafe is required becquse we are working with a raw pointer.
    let kernel_data = unsafe {
        core::slice::from_raw_parts_mut(kernel_buffer.as_ptr(), kernel_size)
    };

    // We read the entire kernel file into our buffer.
    kernel_file.read(kernel_data).unwrap();

    // Confirm that the kernel was loaded from disk successfully.
    uefi::system::with_stdout(|stdout| {
        stdout.output_string(cstr16!("Kernel loaded from disk OK\r\n")).unwrap();
    });

    // -----------------------------------------------------
    // STEP 4 : Parse the ELF file and load segments into memory
    // -----------------------------------------------------
    // The kernel.elf file is not just raw code.
    // It contains headers that describe how to load it into memory
    // We use xmas-elf to parse these headers.

    // We parse the ELF file from our buffer.
    let elf = ElfFile::new(kernel_data).unwrap();

    // We iterate over all program headers.
    // Each program header describes one segment of the kernel.
    // A segment is a contiguous block of code or data.
    for segment in elf.program_iter() {
        
        // We only care about LOAD segments.
        // LOAD segments are the ones that must be copied into memory.
        // Other types (DYNAMIC, NOTE, etc.) are not relevant for us.
        if segment.get_type().unwrap() != Type::Load {
            continue;
        }

        // Where in memory should this segment be place ?
        // physical_addr() gives us the target address in RAM.
        let phys_addr = segment.physical_addr() as *mut u8;

        // What is the raw data of this segment in the ELF file ?
        let data = match segment.get_data(&elf).unwrap() {
            SegmentData::Undefined(data) => data,
            _ => continue,
        };

        // We copy the segment data to the correct memory address.
        // unsafe is required because we are writing to a raw pointer.
        // copy_nonoverlapping is like memcpy in C.
        // It copies bytes from the ELF file into the target RAM address.
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                phys_addr,
                data.len(),
            );
        }
    }

    // Confirm that ELF segments were loaded successfully.
    uefi::system::with_stdout(|stdout| {
        stdout.output_string(cstr16!("Kernel ELF segments loaded OK.\r\n")).unwrap()
    });

    // We get the kernel entry point address from the ELF header.
    // This is the address where we will jump to start the kernel.
    let entry_point = elf.header.pt2.entry_point() as *const ();

    // -----------------------------------------------------
    // STEP 5 : Get the memory map
    // -----------------------------------------------------
    // We must do this right before exiting UEFI boot services.
    // The memory map must be as up to date as possible.
    // The memory map is the list of ALL memory zones in the computer.
    // Each zone has a type : free, used by firmware, reserved by hardware...
    // We need this list because the kernel will need to know which memory
    // zones are free to use, and which ones to avoid.
    // We must get this before we exit UEFI boot services (Step 4), because 
    // after that, we can no longer ask the firmware for anything.

    // MemoryType::LOADER_DATA tells UEFI : "store the memory map itself in
    // a zone of type LOADER_DATA". This type means "data used by the bootloader".
    // The kernel will know to look there.
    let _memory_map = uefi::boot::memory_map(MemoryType::LOADER_DATA).unwrap();
    
    // Just to confirm that the memory map was retrieved successfully.
    // This is the last thing we display via UEFI services.
    // After Step 4, we no longer have access to the screen this way.
    uefi::system::with_stdout(|stdout| {
        stdout.output_string(cstr16!("Memory map OK !\r\n")).unwrap();
        stdout.output_string(cstr16!("Now exiting UEFI boot services...\r\n")).unwrap();
    });

    // -----------------------------------------------------
    // STEP 6 : Exit UEFI boot services
    // -----------------------------------------------------
    // This is the point of no return.
    // We tell the firmware "thank you, you can leave now."
    // After this line :
    // - No more access to UEFI screen services
    // - No more access to UEFI memory services
    // - The CPU belongs entirely to us
    // - The kernel takes full control
    //
    // exit_boot_services() needs the memory map we got in Step 3.
    // It uses it to know exactly what memory it can free up when
    // the firmware shuts down its own services.
    //
    // The underscore bebofre memory_map tells Rust "I know this
    // variable exists but I don't use it yet."
    // We will use it in Step 5 to pass it to the kernel.
    let _memory_map = unsafe { uefi::boot::exit_boot_services(MemoryType::LOADER_DATA) };

    // -----------------------------------------------------
    // Step 7 : Jump to the kernel
    // -----------------------------------------------------
    // We convert the entry point address into a function pointer.
    // A function pointer is a variable that contains the address
    // of a function in memory. We can then call it like a normal function.
    //
    // unsafe is required because we are calling a raw function pointer
    // The compiler has no way to verify that this address is valid.
    // We trust the ELF header to give us the correct entry point.
    unsafe {
        let kernel_entry: extern "C" fn() -> ! = 
            core::mem::transmute(entry_point);
        
        // We call the kernel entry point.
        // This transfers control to the kernel permanently.
        // The bootloader never gets control back after this line.
        kernel_entry();
    }

}

// -----------------------------------------------------
// PANIC HANDLER
// -----------------------------------------------------
// In no_std mode, Rust requires us to define what happens
// when the program panics (crashes).
// Here we simply loop forever.
// Later we could display an error message on screen.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}