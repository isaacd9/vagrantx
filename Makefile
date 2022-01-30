.PHONY: release

debug:
	cargo build --target aarch64-apple-darwin
	codesign --entitlements virtualization_rs.entitlements -s - target/aarch64-apple-darwin/debug/vagrantx
