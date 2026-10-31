# Ready-to-copy app for a local install on an aarch64 device (Raspberry Pi 3/4/5 with 64-bit HA OS).
# The binary is compiled here in Docker, so the device only copies it instead of compiling Rust,
# which takes long and can starve a Pi of memory.
dist:
	rm -rf dist && mkdir -p dist/airprint_bridge
	docker build --platform linux/arm64 --target build -t airprint-bridge-build airprint_bridge
	docker create --name airprint-bridge-extract airprint-bridge-build >/dev/null
	docker cp airprint-bridge-extract:/src/target/release/airprint-bridge dist/airprint_bridge/airprint-bridge
	docker rm airprint-bridge-extract >/dev/null
	cp -R airprint_bridge/translations airprint_bridge/DOCS.md dist/airprint_bridge/
	sed -e '/^  - amd64$$/d' airprint_bridge/config.yaml > dist/airprint_bridge/config.yaml
	printf '%s\n' 'FROM scratch' 'COPY airprint-bridge /airprint-bridge' 'ENTRYPOINT ["/airprint-bridge"]' 'LABEL io.hass.type="addon"' > dist/airprint_bridge/Dockerfile
	@ls -la dist/airprint_bridge

.PHONY: dist
