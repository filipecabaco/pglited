.PHONY: all clean build-release build-release-compressed build-all test compare-sizes help

BINARY_NAME=pglited

all: build-release

build-release:
	@echo "Building optimized release binary..."
	cargo build --release
	strip target/release/$(BINARY_NAME)
	@echo "Binary built: target/release/$(BINARY_NAME)"

build-release-compressed: build-release
	@echo "Compressing binary with UPX..."
	upx --best --lzma target/release/$(BINARY_NAME)
	mv target/release/$(BINARY_NAME) target/release/$(BINARY_NAME)_compressed
	@echo "Compressed binary built: target/release/$(BINARY_NAME)_compressed"

build-all: build-release build-release-compressed

test:
	cargo test

compare-sizes:
	@echo "=== Binary Sizes ==="
	@if [ -f target/release/$(BINARY_NAME) ]; then \
		ls -lh target/release/$(BINARY_NAME) | awk '{print "Uncompressed: " $$5 " (" $$9 ")"}'; \
	fi
	@if [ -f target/release/$(BINARY_NAME)_compressed ]; then \
		ls -lh target/release/$(BINARY_NAME)_compressed | awk '{print "Compressed: " $$5 " (" $$9 ")"}'; \
	fi
	@echo ""
	@echo "=== Disk Usage ==="
	@du -sh target/release/$(BINARY_NAME)* 2>/dev/null || echo "No release builds found"

help:
	@echo "pglited Build Targets"
	@echo ""
	@echo "Build Targets:"
	@echo "  all                     - Build optimized release binary (default)"
	@echo "  build-release           - Build optimized release binary"
	@echo "  build-release-compressed - Build UPX-compressed release binary"
	@echo "  build-all               - Build both release variants"
	@echo ""
	@echo "Testing & Maintenance:"
	@echo "  test          - Run all tests"
	@echo "  clean         - Clean build artifacts"
	@echo "  compare-sizes - Show binary sizes and disk usage"
	@echo "  help          - Show this help message"
	@echo ""
	@echo "Examples:"
	@echo "  make                       # Build release binary"
	@echo "  make build-release-compressed  # Build compressed binary"
	@echo "  make test                  # Run tests"

clean:
	@echo "Cleaning pglited..."
	@cargo clean
