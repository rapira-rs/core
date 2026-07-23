# Distro-neutral PHP discovery: everything derives from php-config (override with
# PHP_CONFIG=/path/to/php-config). Layouts differ per OS — lib vs lib64 (Fedora),
# lib/phpXX (Alpine), plain libphp.so vs versioned libphpX.Y.so (Debian/Ubuntu),
# libphp.dylib (macOS) — so the embed lib is located under the php-config prefix and
# normalized into target/phplib for the linker (-lphp needs the plain name).
PHP_CONFIG ?= php-config
LOCATE_PHP = PREFIX=$$($(PHP_CONFIG) --prefix 2>/dev/null); test -n "$$PREFIX" || { echo "$(PHP_CONFIG) not found; set PHP_CONFIG=/path/to/php-config"; exit 1; }; LIBPHP=$$(find "$$PREFIX/lib64" "$$PREFIX/lib" "$$PREFIX"/lib/php* -maxdepth 1 \( -name 'libphp*.so' -o -name 'libphp*.dylib' \) 2>/dev/null | head -1); test -n "$$LIBPHP" || { echo "no libphp*.so/.dylib under $$PREFIX; install your distro's PHP embed package or build PHP with --enable-embed=shared"; exit 1; }; LIBDIR=$$(dirname "$$LIBPHP"); mkdir -p target/phplib || exit 1; case "$$LIBPHP" in *.dylib) ln -sf "$$LIBPHP" target/phplib/libphp.dylib || exit 1;; *) ln -sf "$$LIBPHP" target/phplib/libphp.so || exit 1;; esac; PHPLIB="$$PWD/target/phplib"

# Regenerate crates/php_sys/rapira_arginfo.h from rapira.stub.php. The generated
# header is committed, so only maintainers editing the stub need this. gen_stub
# unpacks PHP-Parser next to itself on first run (network) and the installed copy
# sits in a root-owned tree, so run from a writable copy under target/.
GEN_STUB ?= $(shell $(PHP_CONFIG) --prefix)/lib/php/build/gen_stub.php

.PHONY: test test_nts test_e2e coverage stubs

stubs:
	@test -f "$(GEN_STUB)" || { echo "gen_stub.php not found at $(GEN_STUB); set GEN_STUB=/path/to/gen_stub.php"; exit 1; }
	@mkdir -p target/stubgen
	@cp "$(GEN_STUB)" target/stubgen/gen_stub.php
	@php target/stubgen/gen_stub.php crates/php_sys/rapira.stub.php

# Sequential recipe (not prerequisites) so `make -j` cannot run the suites
# concurrently; the e2e servers must not overlap the in-process PHP tests.
test:
	@$(MAKE) test_nts
	@$(MAKE) test_e2e

# In-process unit + integration suites. The e2e target is feature-gated off, so
# `cargo test --workspace` skips it here.
test_nts:
	@$(LOCATE_PHP); \
	CARGO_TARGET_DIR=target/nts \
	PHP_CONFIG=$(PHP_CONFIG) \
	LD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	DYLD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	RUSTFLAGS="-L native=$$PHPLIB" \
	cargo test --workspace

# The spawn-the-binary end-to-end suite (crates/integration_tests, --features e2e):
# forks workers, binds ports, drives real HTTP, asserts signal/reload/scaling. Run
# separately so the forking servers do not oversubscribe the in-process PHP tests.
# Builds the rapira bin first; the harness locates it beside the test binary.
test_e2e:
	@$(LOCATE_PHP); \
	CARGO_TARGET_DIR=target/nts \
	PHP_CONFIG=$(PHP_CONFIG) \
	LD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	DYLD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	RUSTFLAGS="-L native=$$PHPLIB" \
	cargo build -p rapira_core --bin rapira && \
	CARGO_TARGET_DIR=target/nts \
	PHP_CONFIG=$(PHP_CONFIG) \
	LD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	DYLD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	RUSTFLAGS="-L native=$$PHPLIB" \
	cargo test -p integration_tests --test e2e --features e2e -- --test-threads=1

# Requires: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview
coverage:
	@$(LOCATE_PHP); \
	CARGO_TARGET_DIR=target/coverage \
	PHP_CONFIG=$(PHP_CONFIG) \
	LD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	DYLD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	RUSTFLAGS="-L native=$$PHPLIB" \
	cargo llvm-cov --workspace --lcov --output-path lcov.info \
		--ignore-filename-regex '(crates/integration_tests/|bindings\.rs$$|/src/main\.rs$$)'
