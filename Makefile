.PHONY: default build test clean run build_ci lint lint_md check_nix e2e e2e_no_kvm build_release release build_x86_64 build_aarch64

amd64_target := x86_64
arm64_target := aarch64
binary := systemd_traefik_configuration_provider
# cargo, rumdl and the other tools come from the dev shell. Recipes that need them enter it, unless make already runs
# inside it (direnv, `nix develop`), which the dev shell marks with TRAEFIK_PROVIDER_DEV_SHELL.
dev_shell := $(if $(TRAEFIK_PROVIDER_DEV_SHELL),,nix develop --command)
rust_deps = $(shell git ls-files --cached --modified --others --exclude-standard '*.rs' Cargo.toml Cargo.lock | sort | uniq | grep -v -e '^\..*' -e '.*\.md' -e Makefile | while IFS= read -r f; do [ -e "$$f" ] && echo "$$f"; done)

default: release

build:
	$(dev_shell) cargo build

test:
	$(dev_shell) cargo nextest run

clean:
	$(dev_shell) cargo clean

run:
	$(dev_shell) cargo run

build_ci:
	@if [ ! -f .forgejo/workflows/.secrets ]; then echo "Secrets file missing at .forgejo/workflows/.secrets"; exit 1; fi
	. .forgejo/workflows/.secrets && $(dev_shell) forgejo-runner exec -W .forgejo/workflows/build.yaml --secret CACHIX_AUTH_TOKEN

lint:
	$(dev_shell) cargo clippy --all-targets --all-features -- -D warnings

lint_md:
	$(dev_shell) rumdl check

# the tests and clippy, built by Nix for the release target
check_nix:
	nix build .#$(amd64_target)_test .#$(amd64_target)_clippy --print-build-logs

# end-to-end test in a NixOS VM, needs KVM
e2e:
	nix build .#checks.x86_64-linux.e2e --print-build-logs

# the same test emulated, for machines (and CI runners) without KVM
e2e_no_kvm:
	nix build .#checks.x86_64-linux.e2e-no-kvm --print-build-logs

build_release:
	$(dev_shell) cargo build --release

target/tmp/$(binary)_$(amd64_target): $(rust_deps)
	nix build .#$(amd64_target) --print-build-logs
	mkdir -p target/tmp
	cp -f result/bin/$(binary)_$(amd64_target) target/tmp/

target/tmp/$(binary)_$(arm64_target): $(rust_deps)
	nix build .#$(arm64_target) --print-build-logs
	mkdir -p target/tmp
	cp -f result/bin/$(binary)_$(arm64_target) target/tmp/

build_x86_64: target/tmp/$(binary)_$(amd64_target)

build_aarch64: target/tmp/$(binary)_$(arm64_target)

release: build_x86_64 build_aarch64
