OVMF := /usr/share/OVMF/OVMF_CODE_4M.fd
IMG := target/kernos.img
EFI := target/x86_64-unknown-uefi/release/bootloader.efi

.PHONY: build run clean

build:
	cargo +nightly build --release

run: build
	mkdir -p target/esp/EFI/BOOT
	cp $(EFI) target/esp/EFI/BOOT/BOOTX64.EFI
	dd if=/dev/zero of=$(IMG) bs=1M count=64 2>/dev/null
	mkfs.fat -F 32 $(IMG) > /dev/null
	mcopy -i $(IMG) -s target/esp/EFI ::EFI
	qemu-system-x86_64 \
		-drive if=pflash,format=raw,readonly=on,file=$(OVMF) \
		-drive format=raw,file=$(IMG) \
		-m 256M \
		-name "KernOS" \
		-nographic

clean:
	cargo clean
	rm -rf target/esp $(IMG)
