# Embed lib layout differs per OS (lib vs lib64, lib/phpXX, versioned or .dylib names), so it is located under the php-config prefix and symlinked into target/phplib under the plain name -lphp needs.
PHP_CONFIG ?= php-config
LOCATE_PHP = PREFIX=$$($(PHP_CONFIG) --prefix 2>/dev/null); test -n "$$PREFIX" || { echo "$(PHP_CONFIG) not found; set PHP_CONFIG=/path/to/php-config"; exit 1; }; LIBPHP=$$(find "$$PREFIX/lib64" "$$PREFIX/lib" "$$PREFIX"/lib/php* -maxdepth 1 \( -name 'libphp*.so' -o -name 'libphp*.dylib' \) 2>/dev/null | head -1); test -n "$$LIBPHP" || { echo "no libphp*.so/.dylib under $$PREFIX; install your distro's PHP embed package or build PHP with --enable-embed=shared"; exit 1; }; LIBDIR=$$(dirname "$$LIBPHP"); mkdir -p target/phplib || exit 1; case "$$LIBPHP" in *.dylib) ln -sf "$$LIBPHP" target/phplib/libphp.dylib || exit 1;; *) ln -sf "$$LIBPHP" target/phplib/libphp.so || exit 1;; esac; PHPLIB="$$PWD/target/phplib"

# gen_stub emits arginfo for the PHP it ships with, so it runs under that build and from a writable copy under target/ (it unpacks PHP-Parser next to itself).
GEN_STUB ?= $(shell $(PHP_CONFIG) --prefix)/lib/php/build/gen_stub.php
PHP_BIN ?= $(shell $(PHP_CONFIG) --prefix)/bin/php

.PHONY: test test_nts test_e2e coverage stubs php

stubs:
	@test -f "$(GEN_STUB)" || { echo "gen_stub.php not found at $(GEN_STUB); set GEN_STUB=/path/to/gen_stub.php"; exit 1; }
	@test -x "$(PHP_BIN)" || { echo "php not found at $(PHP_BIN); set PHP_BIN=/path/to/php"; exit 1; }
	@mkdir -p target/stubgen
	@cp "$(GEN_STUB)" target/stubgen/gen_stub.php
	@for stub in crates/php_sys/*.stub.php; do \
		$(PHP_BIN) target/stubgen/gen_stub.php "$$stub" || exit 1; \
	done

# Sequential recipe, not prerequisites, so make -j cannot overlap the e2e servers with the in-process PHP tests.
test:
	@$(MAKE) test_nts
	@$(MAKE) test_e2e

# The e2e suite is feature-gated off, so cargo test --workspace skips it here.
test_nts:
	@$(LOCATE_PHP); \
	CARGO_TARGET_DIR=target/nts \
	PHP_CONFIG=$(PHP_CONFIG) \
	LD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	DYLD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	RUSTFLAGS="-L native=$$PHPLIB" \
	cargo test --workspace

# Runs apart from test_nts so the forking servers do not oversubscribe the in-process PHP tests; the bin is built first because the harness locates it beside the test binary.
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
	cargo test -p tests --test e2e --features e2e -- --test-threads=1

# Rebuilds the embed PHP from php-src with CI's flag set: distclean wipes configure caches a flag change invalidates, and the macOS SDK iconv override comes after the file flags so autoconf last-wins applies.
PHP_SRC ?= ../../third-party/php-src
PHP_PREFIX ?= $(HOME)/.local/share/php-nts

php:
	@test -d "$(PHP_SRC)" || { echo "php-src not found at $(PHP_SRC); set PHP_SRC=/path/to/php-src"; exit 1; }
	-@$(MAKE) -C "$(PHP_SRC)" distclean >/dev/null 2>&1
	@FLAGS="$$(tr '\n' ' ' < .github/php-configure-flags.txt)"; \
	EXTRA=""; \
	if [ "$$(uname)" = "Darwin" ]; then \
		export PKG_CONFIG_PATH="$$(brew --prefix openssl@3)/lib/pkgconfig:$$(brew --prefix curl)/lib/pkgconfig:$$(brew --prefix oniguruma)/lib/pkgconfig:$$(brew --prefix libxml2)/lib/pkgconfig:$$(brew --prefix sqlite)/lib/pkgconfig:$$(brew --prefix libffi)/lib/pkgconfig$${PKG_CONFIG_PATH:+:$$PKG_CONFIG_PATH}"; \
		EXTRA="--with-iconv=$$(xcrun --show-sdk-path)/usr --with-gettext=$$(brew --prefix gettext)"; \
	fi; \
	cd "$(PHP_SRC)" && \
	./buildconf --force && \
	./configure --prefix="$(PHP_PREFIX)" $$FLAGS $$EXTRA
	@$(MAKE) -C "$(PHP_SRC)" -j"$$(getconf _NPROCESSORS_ONLN)"
	@$(MAKE) -C "$(PHP_SRC)" install

# Requires: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview
coverage:
	@$(LOCATE_PHP); \
	CARGO_TARGET_DIR=target/coverage \
	PHP_CONFIG=$(PHP_CONFIG) \
	LD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	DYLD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" \
	RUSTFLAGS="-L native=$$PHPLIB" \
	cargo llvm-cov --workspace --lcov --output-path lcov.info \
		--ignore-filename-regex '(crates/tests/|bindings\.rs$$|/src/main\.rs$$)'
