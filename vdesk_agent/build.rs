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

    // vcpkg root 탐색: 환경변수 → C:\vcpkg 순서로 확인
    let vcpkg_root = std::env::var("VCPKG_ROOT")
        .unwrap_or_else(|_| "C:\\vcpkg".to_string());

    // x64-windows-static: 설치된 트리플렛으로 변경
    let triplet = "x64-windows-static";
    let installed = format!("{}\\installed\\{}", vcpkg_root, triplet);
    let lib_dir   = format!("{}\\lib", installed);
    let inc_dir   = format!("{}\\include", installed);

    // libvpx 정적 링크
    println!("cargo:rustc-link-search=native={}", lib_dir);
    println!("cargo:rustc-link-lib=static=vpx");

    // vpx가 내부적으로 사용하는 Windows 시스템 라이브러리
    println!("cargo:rustc-link-lib=Winmm");

    // C 래퍼 컴파일 (cc 크레이트 → MSVC cl.exe 자동 사용)
    cc::Build::new()
        .file("src/vpx_wrap.c")
        .include(&inc_dir)
        .opt_level(2)
        .compile("vpx_wrap");

    println!("cargo:rerun-if-changed=src/vpx_wrap.c");
}
