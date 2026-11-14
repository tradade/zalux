fn main() {
    // Nhúng tag release (linux-v*) vào binary: auto-update cần biết bản hiện
    // tại. CI đặt env APP_RELEASE_TAG=${GITHUB_REF_NAME}; build local không
    // đặt → tag rỗng → kiểm tra cập nhật bị tắt.
    let tag = option_env!("APP_RELEASE_TAG").unwrap_or("");
    println!("cargo:rustc-env=APP_RELEASE_TAG={tag}");
    tauri_build::build()
}
