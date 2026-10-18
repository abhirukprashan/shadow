.PHONY: build debug test clean install

build:
	cargo build --release --manifest-path shadow/Cargo.toml

debug:
	cargo build --manifest-path shadow/Cargo.toml

test:
	cargo test --manifest-path shadow/Cargo.toml

clean:
	cargo clean --manifest-path shadow/Cargo.toml
	rm -rf .shadow .shadow-workspaces .shadow-staging staging 2>/dev/null || true

install: build
	@INSTALL_DIR=$(HOME)/.local/bin; \
	mkdir -p $$INSTALL_DIR; \
	cp shadow/target/release/shadow $$INSTALL_DIR/shadow 2>/dev/null || cp shadow/target/release/shadow $$INSTALL_DIR/; \
	chmod +x $$INSTALL_DIR/shadow; \
	echo "Installed to $$INSTALL_DIR/shadow"
