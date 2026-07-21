# Distro-neutral PHP discovery: everything derives from php-config (override with
# PHP_CONFIG=/path/to/php-config). Layouts differ per OS — lib vs lib64 (Fedora),
# lib/phpXX (Alpine), plain libphp.so vs versioned libphpX.Y.so (Debian/Ubuntu),
# libphp.dylib (macOS) — so the embed lib is located under the php-config prefix and
# normalized into target/phplib for the linker (-lphp needs the plain name).
PHP_CONFIG ?= php-config
LOCATE_PHP = PREFIX=$$($(PHP_CONFIG) --prefix 2>/dev/null); test -n "$$PREFIX" || { echo "$(PHP_CONFIG) not found; set PHP_CONFIG=/path/to/php-config"; exit 1; }; LIBPHP=$$(find "$$PREFIX/lib64" "$$PREFIX/lib" "$$PREFIX"/lib/php* -maxdepth 1 \( -name 'libphp*.so' -o -name 'libphp*.dylib' \) 2>/dev/null | head -1); test -n "$$LIBPHP" || { echo "no libphp*.so/.dylib under $$PREFIX; install your distro's PHP embed package or build PHP with --enable-embed=shared"; exit 1; }; LIBDIR=$$(dirname "$$LIBPHP"); mkdir -p target/phplib || exit 1; case "$$LIBPHP" in *.dylib) ln -sf "$$LIBPHP" target/phplib/libphp.dylib || exit 1;; *) ln -sf "$$LIBPHP" target/phplib/libphp.so || exit 1;; esac; PHPLIB="$$PWD/target/phplib"

.PHONY: test test_nts coverage

test: test_nts

test_nts:
	@$(LOCATE_PHP); \
	CARGO_TARGET_DIR=target/nts \
	PHP_CONFIG=$(PHP_CONFIG) \
	LD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	DYLD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	RUSTFLAGS="-L native=$$PHPLIB" \
	cargo test --workspace

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
