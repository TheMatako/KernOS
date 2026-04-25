/* ============================================================
   KernOS Kernel Linker Script
   ============================================================
   
   This script tells the linker where to place each part
   of the kernel in physical memory.
   
   We place the kernel at 1 MB (0x100000).
   This is a conventional safe address for a kernel :
   - Below 1 MB is reserved for BIOS/UEFI and hardware.
   - At and above 1 MB is free for our use.
   ============================================================ */

ENTRY(kernel_main)

SECTIONS {
    /* Start at 1 MB physical address */
    . = 0x100000;

    /* Code section : all executable code goes here */
    .text : {
        *(.text .text.*)
    }

    /* Read-only data : constants, string literals */
    .rodata : {
        *(.rodata .rodata.*)
    }

    /* Initialized data : global variables with a default value */
    .data : {
        *(.data .data.*)
    }

    /* Zero-initialized data : global variables set to zero at startup */
    .bss : {
        __bss_start = .;
        *(.bss .bss.*)
        __bss_end = .;
    }
}