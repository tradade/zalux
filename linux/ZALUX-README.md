# Zalux — bản dựng Olaz Linux

Repo này chỉ chứa **bản dựng (AppImage)** của [Olaz Linux](../zalo-swift-re) —
vỏ Tauri chạy renderer chính thức của Zalo PC trên Linux. Source code nằm ở
repo nguồn; repo này chỉ phát hành file cài đặt.

> **Unofficial project, không liên quan tới VNG.** "Zalo" là thương hiệu của
> VNG. Dự án thuộc nhóm nghiên cứu tương thích / reverse-engineering; dùng
> client không chính thức có thể mâu thuẫn ToS của Zalo — dùng với tài khoản
> của bạn và tự chịu rủi ro.

## Tải về

Vào tab **[Releases](../../releases)**, tải:

- `Olaz-<tag>-amd64.AppImage` — bản dựng chính (≈ 90 MB)
- `SHA256SUMS` — kiểm tra toàn vẹn (tuỳ chọn nhưng khuyên dùng):

```bash
sha256sum -c SHA256SUMS
```

## Chạy lần đầu

```bash
chmod +x Olaz-*-amd64.AppImage
./Olaz-*-amd64.AppImage
```

- **Lần chạy đầu app tự tải runtime Zalo PC từ VNG** (~323 MB) và đối chiếu
  SHA-256 của bản ghim — repo này **không phân phối code VNG**. Cần
  `p7zip-full` để giải nén.
- Đăng nhập bằng QR từ Zalo trên điện thoại.
- **Đồng bộ tin nhắn Điện thoại → PC**: Cài đặt → Đồng bộ tin nhắn, hoặc
  `curl -X POST localhost:27182/__olaz/transfer/start` (xem `linux/README.md`
  trong repo nguồn).

## Yêu cầu hệ thống

- Linux x86_64 (đã test Ubuntu 22.04+; WebKitGTK 4.1)
- Chạy từ AppImage không cần cài gì — thư viện WebKitGTK phải có trên hệ thống
  (hầu hết distro desktop đã có; Ubuntu: `libwebkit2gtk-4.1-0`)

## Bản dựng được tạo thế nào

Workflow GitHub Actions trong repo nguồn
(`.github/workflows/linux-release.yml`):

1. Push tag `linux-v*` lên repo nguồn (hoặc chạy tay `workflow_dispatch`).
2. Runner Ubuntu cài WebKitGTK 4.1 dev + Rust stable + Go, chạy
   `linux/scripts/build-appimage.sh` — build từ source, **không dùng artefact
   cục bộ**.
3. AppImage + `SHA256SUMS` được đính lên release của repo này (tag cùng tên).
   Workflow cần secret `ZALUX_RELEASE_TOKEN` (PAT có quyền Contents: write
   trên repo này) — chỉ maintainer của repo nguồn cần cấu hình một lần.

Mỗi release ghi rõ nguồn build (repo@commit) — ai cũng tái hiện được bằng
cùng một script.

## Cập nhật

App hiện tự cập nhật **runtime Zalo** (tải bản mới từ VNG khi có). Bản
AppImage mới theo release ở repo này — ghé Releases để kiểm tra.
