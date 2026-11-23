.PHONY: build build-windows install test clean

# Where `make install` puts vox. ~/.local/bin is on PATH by default on Fedora;
# override with `make install BINDIR=/some/dir`.
BINDIR ?= $(HOME)/.local/bin

# Thin convenience wrapper: Cargo stays the build system (docs/DESIGN.md §8).
build:
	cargo build --release

# Cross-compile vox.exe for Windows from Linux (docs/DESIGN.md §8) via the MinGW
# toolchain: target/x86_64-pc-windows-gnu/release/vox.exe. Needs
# `rustup target add x86_64-pc-windows-gnu`, mingw64-gcc, and cmake.
# The env vars force static bundled libopus: audiopus_sys picks linkage from the
# build machine (Linux -> dynamic + pkg-config), not from the Windows target.
WIN_TARGET := x86_64-pc-windows-gnu
build-windows:
	LIBOPUS_STATIC=1 LIBOPUS_NO_PKG=1 cargo build --release --target $(WIN_TARGET)
	@echo "built target/$(WIN_TARGET)/release/vox.exe"

# Linux: build vox and copy it onto PATH, replacing any older copy. Run it again
# after every change to update. Config files are not touched.
install: build
	install -Dm755 target/release/vox $(BINDIR)/vox
	@$(BINDIR)/vox --version

test:
	cargo test

clean:
	cargo clean
