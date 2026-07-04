ZTS_PHP_CONFIG := $(HOME)/.local/php-zts/bin/php-config
ZTS_LIB        := $(HOME)/.local/php-zts/lib
NTS_PHP_CONFIG := /usr/bin/php-config
NTS_LIB        := /usr/lib64

.PHONY: test test_zts test_nts coverage

test: test_zts test_nts

test_zts:
	CARGO_TARGET_DIR=target/zts \
	PHP_CONFIG=$(ZTS_PHP_CONFIG) \
	LD_LIBRARY_PATH=$(ZTS_LIB) \
	cargo test --workspace

test_nts:
	@test -f $(NTS_LIB)/libphp.so || { echo "NTS embed lib missing -> sudo dnf install php-embedded"; exit 1; }
	CARGO_TARGET_DIR=target/nts \
	PHP_CONFIG=$(NTS_PHP_CONFIG) \
	LD_LIBRARY_PATH=$(NTS_LIB) \
	RUSTFLAGS="-L native=$(NTS_LIB)" \
	cargo test --workspace

# Requires: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview
coverage:
	CARGO_TARGET_DIR=target/coverage \
	PHP_CONFIG=$(ZTS_PHP_CONFIG) \
	LD_LIBRARY_PATH=$(ZTS_LIB) \
	cargo llvm-cov --workspace --lcov --output-path lcov.info \
		--ignore-filename-regex '(crates/integration_tests/|bindings\.rs$$|/src/main\.rs$$)'
