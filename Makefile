QEMU_TAG    := $(shell python3 -c 'import json;print(json.load(open("qemu/upstream.lock.json"))["tag"])')
QEMU_COMMIT := $(shell python3 -c 'import json;print(json.load(open("qemu/upstream.lock.json"))["commit"])')

.PHONY: help doctor qemu check
help:            ## list targets
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | sed 's/:.*##/\t/'
doctor:          ## check host tools and QEMU
	cargo run -q --locked -p sbc-cli -- doctor --board pi4b
qemu:            ## fetch pinned QEMU and build it into build/qemu
	test -d build/qemu-src || git clone --depth 1 --branch $(QEMU_TAG) https://gitlab.com/qemu-project/qemu.git build/qemu-src
	test "$$(git -C build/qemu-src rev-parse HEAD)" = "$(QEMU_COMMIT)"
	mkdir -p build/qemu && cd build/qemu && ../qemu-src/configure --target-list=aarch64-softmmu,arm-softmmu --disable-docs --disable-werror --python=$$(command -v python3.12 || command -v python3) && ninja
check:           ## format, lint, unit tests
	cargo fmt --all -- --check
	cargo clippy --locked --workspace --all-targets -- -D warnings
	cargo test --locked --workspace
