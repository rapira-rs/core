# Distro-neutral PHP discovery: everything derives from php-config (override with
# PHP_CONFIG=/path/to/php-config). Distros differ in lib dir (lib vs lib64) and lib
# naming (libphp.so vs versioned libphpX.Y.so), so the embed lib is located under the
# php-config prefix and normalized to target/phplib/libphp.so for the linker.
PHP_CONFIG ?= php-config
LOCATE_PHP = PREFIX=$$($(PHP_CONFIG) --prefix 2>/dev/null); test -n "$$PREFIX" || { echo "$(PHP_CONFIG) not found; set PHP_CONFIG=/path/to/php-config"; exit 1; }; LIBPHP=$$(find "$$PREFIX/lib64" "$$PREFIX/lib" -maxdepth 1 -name 'libphp*.so' 2>/dev/null | head -1); test -n "$$LIBPHP" || { echo "no libphp*.so under $$PREFIX (lib/lib64); install your distro's PHP embed package or build PHP with --enable-embed=shared"; exit 1; }; mkdir -p target/phplib; ln -sf "$$LIBPHP" target/phplib/libphp.so; PHPLIB="$$PWD/target/phplib"; LIBDIR=$$(dirname "$$LIBPHP")

.PHONY: test test_nts coverage

test: test_nts

test_nts:
	@$(LOCATE_PHP); \
	CARGO_TARGET_DIR=target/nts \
	PHP_CONFIG=$(PHP_CONFIG) \
	LD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	RUSTFLAGS="-L native=$$PHPLIB" \
	cargo test --workspace

# Requires: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview
coverage:
	@$(LOCATE_PHP); \
	CARGO_TARGET_DIR=target/coverage \
	PHP_CONFIG=$(PHP_CONFIG) \
	LD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	RUSTFLAGS="-L native=$$PHPLIB" \
	cargo llvm-cov --workspace --lcov --output-path lcov.info \
		--ignore-filename-regex '(crates/integration_tests/|bindings\.rs$$|/src/main\.rs$$)'
