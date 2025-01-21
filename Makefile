
build-rpi:
	cross build --release

deploy-rpi:
	scp target/aarch64-unknown-linux-gnu/release/statime pi@rpi.local:~/

