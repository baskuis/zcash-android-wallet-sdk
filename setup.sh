curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo build
rustup target add armv7-linux-androideabi aarch64-linux-android i686-linux-android
rustup target add x86_64-linux-android
./gradlew build
