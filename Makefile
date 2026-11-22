.PHONY: build install test clean

# Where `make install` puts vox. ~/.local/bin is on PATH by default on Fedora;
# override with `make install BINDIR=/some/dir`.
BINDIR ?= $(HOME)/.local/bin

# Thin convenience wrapper: Cargo stays the build system (docs/DESIGN.md §8).
build:
	cargo build --release

# Linux: build vox and copy it onto PATH, replacing any older copy. Run it again
# after every change to update. Config files are not touched.
install: build
	install -Dm755 target/release/vox $(BINDIR)/vox
	@$(BINDIR)/vox --version

test:
	cargo test

clean:
	cargo clean
