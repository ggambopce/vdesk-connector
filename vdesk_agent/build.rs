fn main() {
    // 배포용 API URL을 바이너리에 고정 (빌드 시 VDESK_API_URL 환경 변수로 지정)
    // 런타임에 VDESK_API_URL 환경 변수가 있으면 override 가능 (개발 편의)
    let api_url = std::env::var("VDESK_API_URL")
        .unwrap_or_else(|_| "http://localhost:8080".to_string());
    println!("cargo:rustc-env=VDESK_API_URL={}", api_url);
    println!("cargo:rerun-if-env-changed=VDESK_API_URL");
}
