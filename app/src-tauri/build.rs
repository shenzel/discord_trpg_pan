use std::{env, fs, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let sdk = env::var("DPAN_SDK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("../../third_party/sdk-1.10.19337/discord_social_sdk"));
    println!("cargo:rerun-if-env-changed=DPAN_SDK_DIR");
    println!("cargo:rerun-if-changed=native/voice_bridge.cpp");
    println!("cargo:rerun-if-changed=native/voice_bridge.h");

    // Social SDK を動かす C++ のブリッジ
    cc::Build::new()
        .cpp(true)
        .std("c++20")
        .flag("/utf-8")
        .flag("/EHsc")
        .include(sdk.join("include"))
        .include("native")
        .file("native/voice_bridge.cpp")
        .compile("voice_bridge");
    println!("cargo:rustc-link-search=native={}", sdk.join("lib/release").display());
    println!("cargo:rustc-link-lib=dylib=discord_partner_sdk");
    // SDK の DLL は使うときに読み込む（テストなど、通話を使わない実行では DLL がなくても動く）
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-arg=/DELAYLOAD:discord_partner_sdk.dll");

    // DLL とモデルファイルを exe の隣へ（target/<profile>/）
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    if let Some(profile_dir) = out.ancestors().nth(3) {
        if let Ok(entries) = fs::read_dir(sdk.join("bin/release")) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_file() {
                    let _ = fs::copy(&p, profile_dir.join(e.file_name()));
                }
            }
        }
    }

    tauri_build::build()
}
