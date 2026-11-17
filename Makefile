.PHONY: build clean

build:
	cargo build --profile release-debug

clean:
	cargo clean
