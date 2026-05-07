cd ~/Desktop/repositories/wisetree
cargo build --release
export PATH="$PWD/target/release:$PATH"
hash -r
source ~/.bash_profile
wisetree
