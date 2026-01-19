.PHONY: default build test clean run build_release release

amd64_target := x86_64
arm64_target := aarch64
binary := systemd_traefik_configuration_provider
rust_deps = $(shell git ls-files --cached --modified --others --exclude-standard '*.rs' Cargo.toml Cargo.lock | sort | uniq | grep -v -e '^\..*' -e '.*\.md' -e Makefile | while IFS= read -r f; do [ -e "$$f" ] && echo "$$f"; done)

default: release

build:
	cargo build

test:
	cargo nextest run

clean:
	cargo clean

run:
	cargo run

build_release:
	cargo build --release

target/tmp/$(binary)_$(amd64_target): $(rust_deps)
	nix build .\#$(amd64_target) --print-build-logs
	mkdir -p target/tmp
	cp -f result/bin/$(binary)_$(amd64_target) target/tmp/

target/tmp/$(binary)_$(arm64_target): $(rust_deps)
	nix build .\#$(arm64_target) --print-build-logs
	mkdir -p target/tmp
	cp -f result/bin/$(binary)_$(arm64_target) target/tmp/

release: target/tmp/$(binary)_$(amd64_target) target/tmp/$(binary)_$(arm64_target)
