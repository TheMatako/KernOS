use core::fmt;
use core::ptr;
use font8x8::UnicodeFonts;
use shared::FramebufferInfo;

/// Global reference to the framebuffer information
static mut FRAMEBUFFER: Option<FramebufferInfo> = None;

/// Terminal cursor position (in pixels)
static mut CURSOR_X: u32 = 0;
static mut CURSOR_Y: u32 = 0;

const SCALE: u32 = 3;
const FONT_WIDTH: u32 = 8 * SCALE;
const FONT_HEIGHT: u32 = 8 * SCALE;

/// Pointer to the off-screen RAM buffer used for double buffering
static mut BACK_BUFFER: *mut u32 = core::ptr::null_mut();

/// Initializes the graphical terminal and clears the screen
pub unsafe fn init(fb_info: FramebufferInfo) {
    FRAMEBUFFER = Some(fb_info);
    clear(0x000000); // Wipe screen to black
}

/// Enables double buffering by allocating a contiguous physical block of RAM.
/// This drastically improves performance by avoiding direct reads/writes to PCIe memory.
pub unsafe fn enable_double_buffering() {
    if let Some(fb_info) = FRAMEBUFFER {
        let size_pixels = (fb_info.stride * fb_info.height) as usize;
        let size_bytes = size_pixels * 4;

        // 1. Calculate the number of 4 KiB pages needed
        // Round up (e.g., 8.3 MB / 4096 = 2025 pages)
        let pages_needed = size_bytes.div_ceil(4096);

        // 2. Request a contiguous block of physical memory directly from the PMM
        let phys_base = crate::pmm::alloc_frames_contiguous(pages_needed)
            .expect("Failed to allocate physical frames for BackBuffer");

        // 3. Obtain the virtual address via the VMM's Direct Map
        let virt_addr = crate::vmm::phys_to_virt(x86_64::PhysAddr::new(phys_base));
        BACK_BUFFER = virt_addr.as_mut_ptr();

        // 4. Copy the current screen content to the BackBuffer so early logs aren't lost
        core::ptr::copy_nonoverlapping(
            fb_info.base as *const u8,
            BACK_BUFFER as *mut u8,
            size_bytes,
        );

        crate::kprintln!(
            "[FB] Double buffering enabled ({} MB allocated).",
            size_bytes / (1024 * 1024)
        );
    }
}

/// Fills the entire screen with a specific color (ARGB/XRGB format)
pub unsafe fn clear(color: u32) {
    if let Some(fb) = FRAMEBUFFER {
        let total_pixels = (fb.stride * fb.height) as usize;

        if !BACK_BUFFER.is_null() {
            // Fast path for clearing RAM buffer
            if color == 0 {
                ptr::write_bytes(BACK_BUFFER as *mut u8, 0, total_pixels * 4);
            } else {
                for i in 0..total_pixels {
                    ptr::write_volatile(BACK_BUFFER.add(i), color);
                }
            }
            blit_screen(); // Push RAM to screen
        } else {
            // Fallback for early boot before double buffering is enabled
            let ptr = fb.base as *mut u32;
            for i in 0..total_pixels {
                ptr::write_volatile(ptr.add(i), color);
            }
        }

        // Reset cursor to top-left corner
        CURSOR_X = 0;
        CURSOR_Y = 0;
    }
}

/// Copies a specific rectangle from the BackBuffer to the FrontBuffer.
/// This prevents full screen redraws when typing single characters.
unsafe fn blit_rect(x: u32, y: u32, width: u32, height: u32) {
    if BACK_BUFFER.is_null() {
        return;
    }
    if let Some(fb) = FRAMEBUFFER {
        let max_y = core::cmp::min(y + height, fb.height);
        let max_x = core::cmp::min(x + width, fb.width);
        if x >= max_x || y >= max_y {
            return;
        }

        let copy_width_bytes = (max_x - x) as usize * 4;

        for row in y..max_y {
            let offset = (row * fb.stride + x) as usize;
            ptr::copy_nonoverlapping(
                (BACK_BUFFER as *const u8).add(offset * 4),
                (fb.base as *mut u8).add(offset * 4),
                copy_width_bytes,
            );
        }
    }
}

/// Copies the entire BackBuffer to the physical screen.
/// Used after a scroll or a full screen clear.
unsafe fn blit_screen() {
    if BACK_BUFFER.is_null() {
        return;
    }
    if let Some(fb) = FRAMEBUFFER {
        let total_bytes = (fb.stride * fb.height) as usize * 4;
        ptr::copy_nonoverlapping(BACK_BUFFER as *const u8, fb.base as *mut u8, total_bytes);
    }
}

/// Draws a single pixel at (x, y) coordinates ONLY to the RAM buffer.
unsafe fn draw_pixel_backbuffer(x: u32, y: u32, color: u32) {
    if let Some(fb) = FRAMEBUFFER {
        if x >= fb.width || y >= fb.height {
            return;
        }

        let offset = (y * fb.stride + x) as usize;

        if !BACK_BUFFER.is_null() {
            ptr::write_volatile(BACK_BUFFER.add(offset), color);
        } else {
            // Fallback for early boot
            let front_ptr = fb.base as *mut u32;
            ptr::write_volatile(front_ptr.add(offset), color);
        }
    }
}

/// Renders a scaled character using the font8x8 bitmap font to the RAM buffer.
unsafe fn draw_char(c: char, x: u32, y: u32, fg_color: u32, bg_color: u32) {
    let bitmap = match font8x8::BASIC_FONTS.get(c) {
        Some(b) => b,
        None => match font8x8::BASIC_FONTS.get('?') {
            Some(b) => b,
            None => return,
        },
    };

    for (row, byte) in bitmap.iter().enumerate() {
        for col in 0..8 {
            let is_pixel_on = (*byte & (1 << col)) != 0;
            let color = if is_pixel_on { fg_color } else { bg_color };

            // Scaling: Draw a SCALE x SCALE square
            for dy in 0..SCALE {
                for dx in 0..SCALE {
                    let screen_x = x + (col as u32 * SCALE) + dx;
                    let screen_y = y + (row as u32 * SCALE) + dy;
                    draw_pixel_backbuffer(screen_x, screen_y, color);
                }
            }
        }
    }
}

/// Shifts all RAM content up by one font row height.
/// Does NOT write to the physical screen.
unsafe fn scroll_backbuffer() {
    if let Some(fb) = FRAMEBUFFER {
        let bytes_per_pixel = 4;
        let stride_bytes = fb.stride as usize * bytes_per_pixel;
        let line_height_bytes = FONT_HEIGHT as usize * stride_bytes;
        let total_size_bytes = (fb.stride * fb.height) as usize * bytes_per_pixel;
        let bytes_to_copy = total_size_bytes - line_height_bytes;

        if BACK_BUFFER.is_null() {
            return;
        }

        // --- STEP 1: Scroll in RAM (Instantaneous) ---
        // Move the data up in the Back Buffer
        ptr::copy_nonoverlapping(
            (BACK_BUFFER as *const u8).add(line_height_bytes),
            BACK_BUFFER as *mut u8,
            bytes_to_copy,
        );

        // --- STEP 2: Clear the bottom line in the Back Buffer ---
        ptr::write_bytes(
            (BACK_BUFFER as *mut u8).add(bytes_to_copy),
            0,
            line_height_bytes,
        );

        CURSOR_Y -= FONT_HEIGHT;
    }
}

/// Implements the core writing logic for the terminal
pub struct TerminalWriter;

impl fmt::Write for TerminalWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        unsafe {
            if let Some(fb) = FRAMEBUFFER {
                // Flag to track if we need a full screen update at the end
                let mut needs_full_blit = false;

                for c in s.chars() {
                    match c {
                        '\n' => {
                            CURSOR_X = 0;
                            CURSOR_Y += FONT_HEIGHT;
                        }
                        '\r' => {
                            CURSOR_X = 0;
                        }
                        _ => {
                            // Handle line wrapping
                            if CURSOR_X + FONT_WIDTH > fb.width {
                                CURSOR_X = 0;
                                CURSOR_Y += FONT_HEIGHT;
                            }

                            // Handle screen scrolling
                            if CURSOR_Y + FONT_HEIGHT > fb.height {
                                scroll_backbuffer();
                                needs_full_blit = true;
                            }

                            // Draw char to RAM (White on Black)
                            draw_char(c, CURSOR_X, CURSOR_Y, 0xFFFFFF, 0x000000);

                            // If no scroll occurred, push just this char to the screen
                            if !needs_full_blit {
                                blit_rect(CURSOR_X, CURSOR_Y, FONT_WIDTH, FONT_HEIGHT);
                            }

                            CURSOR_X += FONT_WIDTH;
                        }
                    }

                    // Final check in case a newline pushed us off-screen
                    if CURSOR_Y + FONT_HEIGHT > fb.height {
                        scroll_backbuffer();
                        needs_full_blit = true;
                    }
                }

                // If a scroll occurred during this batch, update the entire screen once
                if needs_full_blit {
                    blit_screen();
                }
            }
        }
        Ok(())
    }
}

/// Handles the backspace action by moving back and clearing the character.
pub unsafe fn backspace() {
    if CURSOR_X >= FONT_WIDTH {
        CURSOR_X -= FONT_WIDTH;

        // Draw a black square to RAM
        draw_char(' ', CURSOR_X, CURSOR_Y, 0x000000, 0x000000);

        // Push only the cleared rectangle to the screen
        blit_rect(CURSOR_X, CURSOR_Y, FONT_WIDTH, FONT_HEIGHT);
    }
}

/// Helper function to print formatted strings to the graphical terminal
pub fn print(args: fmt::Arguments) {
    use core::fmt::Write;
    let _ = TerminalWriter.write_fmt(args);
}
