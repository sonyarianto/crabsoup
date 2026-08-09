fn main() {
    // fdk-aac is installed from source into /usr/local (no distro package).
    println!("cargo:rustc-link-search=native=/usr/local/lib");
}
