fn main() {
    // 배포용 API URL을 바이너리에 고정 (빌드 시 VDESK_API_URL 환경 변수로 지정)
    // 런타임에 VDESK_API_URL 환경 변수가 있으면 override 가능 (개발 편의)
    let api_url = std::env::var("VDESK_API_URL")
        .unwrap_or_else(|_| "http://localhost:8080".to_string());
    println!("cargo:rustc-env=VDESK_API_URL={}", api_url);
    println!("cargo:rerun-if-env-changed=VDESK_API_URL");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let vcpkg_root = std::env::var("VCPKG_ROOT")
        .unwrap_or_else(|_| "C:\\vcpkg".to_string());

    let triplet   = "x64-windows-static";
    let installed = format!("{}\\installed\\{}", vcpkg_root, triplet);
    let lib_dir   = format!("{}\\lib", installed);
    let inc_dir   = format!("{}\\include", installed);

    println!("cargo:rustc-link-search=native={}", lib_dir);
    println!("cargo:rustc-link-lib=static=vpx");
    println!("cargo:rustc-link-lib=Winmm");

    cc::Build::new()
        .file("src/vpx_wrap.c")
        .include(&inc_dir)
        .opt_level(2)
        .compile("vpx_wrap");

    println!("cargo:rerun-if-changed=src/vpx_wrap.c");
}
