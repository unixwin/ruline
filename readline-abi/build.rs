//! Emit the GNU-compatible shared-object identity.
//!
//! Cargo names the artifact `libreadline.so`; the ELF/`Mach-O` SONAME is what
//! programs linked with `-lreadline` record as their runtime dependency.  Keep
//! it at the GNU ABI major (`libreadline.so.8`) so a `dlopen`ed bash builtin
//! resolves the same library it was compiled against.  The `scripts/package.sh`
//! helper materialises the versioned filename next to the Cargo output.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "linux" | "android" | "freebsd" | "netbsd" | "openbsd" | "dragonfly" => {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libreadline.so.8");
        }
        "macos" | "ios" => {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/libreadline.8.dylib");
        }
        _ => {}
    }
}
