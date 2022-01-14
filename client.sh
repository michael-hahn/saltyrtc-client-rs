# install Rust on Linux
curl https://sh.rustup.rs -sSf | sh
source $HOME/.cargo/env
rustc --version

# install some more dependencies
# You may encounter issues when installing libssl1.0-dev
# You may need to reinstall a different version of libssl (based on the error message). On our test machine, we need to:
# sudo apt-get install --reinstall libssl1.0.0=1.0.2n-1ubuntu5
sudo apt-get install build-essential pkg-config libssl1.0-dev -y

# build the rust program
cargo build

