# ============================================================
# KernOS Build System
# ============================================================
#
# Available commands :
#   make build   - compile bootloader and kernel
#   make run     - compile and launch in QEMU
#   make clean   - delete all compiled files
# ============================================================

# Path to the UEFI firmware used by QEMU.
OVMF := /usr/share/OVMF/OVMF_CODE_4M.fd

# Path to the final disk image.
IMG := target/kernos.img

# Path to the compiled bootloader.
EFI := target/x86_64-unknown-uefi/release/bootloader.efi

# Path to the compiled kernel.
KERNEL := target/x86_64-unknown-none/release/kernel

# All targets listed here are commands.
# Without .PHONY, make would look for files named build, run, clean.
.PHONY: build run clean fmt lint pxe-update

# ------------------------------------------------------------
# build : compile bootloader and kernel
# ------------------------------------------------------------
build:
	@echo "[KernOS] Compiling bootloader..."
	cargo +nightly build -p bootloader --release --target x86_64-unknown-uefi

	@echo "[KernOS] Compiling kernel..."
	cargo +nightly build -p kernel --release --target x86_64-unknown-none

	@echo "[KernOS] Build complete."
	

# ------------------------------------------------------------
# run : build then launch in QEMU
# ------------------------------------------------------------
run: build
	@echo "[KernOS] Building disk image..."
	
	# Create the UEFI directory structure
	# The UEFI firmware always look for the bootloader at EFI/BOOT/BOOTX64.EFI.
	mkdir -p target/esp/EFI/BOOT

	# Copy the bootloader to the correct UEFI path.
	cp $(EFI) target/esp/EFI/BOOT/BOOTX64.EFI

	# Copy the kernel to the root of the partition.
	# The bootloader looks for "kernel.elf" at the root.
	cp $(KERNEL) target/esp/kernel.elf

	# Create a blank 64 MB raw disk image.
	dd if=/dev/zero of=$(IMG) bs=1M count=64 2>/dev/null

	# Format the disk image as FAT32.
	# UEFI requires FAT32 for the EFI system partition.
	mkfs.fat -F 32 $(IMG) > /dev/null

	# Copy our files into the FAT32 image.
	# mcopy works without root privileges, unlike mount.
	mcopy -i $(IMG) -s target/esp/EFI ::EFI
	mcopy -i $(IMG) target/esp/kernel.elf ::kernel.elf

	@echo "[KernOS] Launching QEMU..."
	qemu-system-x86_64 \
		-bios $(OVMF) \
		-drive if=pflash,format=raw,readonly=on,file=$(OVMF) \
		-drive format=raw,file=$(IMG) \
		-m 256M \
		-name "KernOS" \
		-netdev user,id=net0,hostfwd=tcp::8080-:80 \
		-device e1000,netdev=net0 \
		-nographic


# ------------------------------------------------------------
# clean : delete all compiled output
# ------------------------------------------------------------
clean:
	@echo "[KernOS] Cleaning..."
	cargo clean
	rm -rf target/esp $(IMG)
	@echo "[KernOS] Done."

# ------------------------------------------------------------
# fmt : format all code
# ------------------------------------------------------------
fmt:
	@echo "[KernOS] Formatting code..."
	cargo +nightly fmt
	@echo "[KernOS] Done."

# ------------------------------------------------------------
# lint : run clippy on all crates
# ------------------------------------------------------------
lint:
	@echo "[KernOS] Running clippy on bootloader..."
	cargo +nightly clippy \
		-p bootloader \
		--target x86_64-unknown-uefi \
		-- -D warnings

	@echo "[KernOS] Running clippy on kernel..."
	cargo +nightly clippy \
		-p kernel \
		--target x86_64-unknown-none \
		-- -D warnings

# ------------------------------------------------------------
# pxe-update : copy bootloader and kernel to TFTP server
# ------------------------------------------------------------
pxe-update: build
	@echo "[KernOS] Updating PXE TFTP files..."
	sudo cp $(EFI) /srv/tftp/bootx64.efi
	sudo cp $(KERNEL) /srv/tftp/kernel.elf
	sudo chmod 644 /srv/tftp/bootx64.efi /srv/tftp/kernel.elf
	@echo "[KernOS] PXE files updated."