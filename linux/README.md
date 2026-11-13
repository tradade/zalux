# Olaz — vỏ Tauri cho Linux

Bản chạy Olaz trên Linux: một cửa sổ Tauri (WebKitGTK) chứa **renderer chính thức của Zalo PC**
(`app.asar`, do chính VNG phát hành), cộng với phần mà Electron's preload/main từng cung cấp —
bridge `$z*`, proxy mạng, cookie, lưu trữ — viết lại bằng Rust.

## Tải bản dựng sẵn

AppImage chính thức được build tự động và phát hành tại
**[tradade/zalux — Releases](https://github.com/tradade/zalux/releases)** (kèm `SHA256SUMS`).
Repo đó chứa cả source Linux port + workflow build; push tag `linux-v*` là
[`.github/workflows/linux-release.yml`](https://github.com/tradade/zalux/blob/main/.github/workflows/linux-release.yml)
build và đăng release tự động — chi tiết xem [`ZALUX-README.md`](ZALUX-README.md).

Đây là bản port của [bản macOS](../README.md), không phải bản viết lại giao diện: 20 host script
trong `Sources/Olaz/HostScripts/` được nhúng nguyên văn và chạy không sửa một dòng nào.

```
Linux (Tauri / WebKitGTK)
  cửa sổ  ──►  http://127.0.0.1:<port>/app/<page>   ← đọc thẳng từ app.asar (asar.rs)
                 │  mỗi trang HTML được chèn <script src="/__olaz/boot.js"> ngay sau <head>
                 ├─ /__olaz/boot.js     shim + spec + 20 host script, một bundle 626 KB
                 ├─ web/olaz-linux.js   giả lập window.webkit.messageHandlers + XHR zalo-app://
                 ├─ HostScripts/*.js    (nguyên văn từ bản macOS)
                 │     └─ POST /bridge/zbridge | /bridge/znet | /sync   (host.rs)
                 ├─ bridge.rs           $znode · $zelectron · crypto · IPC 450 kênh
                 └─ net.rs              HTTP (reqwest) + WebSocket (tungstenite)
```

## Vì sao bridge chạy qua localhost chứ không qua IPC của Tauri

`HostScripts/bridge.js` có những lời gọi **đồng bộ thật** (XHR blocking). Trên macOS, Swift trả lời
chúng ngay trong URL-scheme handler của `WKWebView`. Trong WebKitGTK, handler của custom scheme chạy
trên cùng thread với XHR đang block → deadlock. Một HTTP server trên loopback trả lời từ thread riêng,
nên lời gọi đồng bộ hoàn tất đúng như trên macOS. Cửa sổ Tauri chỉ lo phần vỏ (cửa sổ, đóng gói).

Hệ quả: trang không gọi IPC nào của Tauri, nên `capabilities/default.json` để trống quyền.

## Chạy

```bash
cargo run --manifest-path linux/src-tauri/Cargo.toml
```

Lần chạy đầu **tự lấy runtime** giống bản macOS: `runtime.rs` tải `ZaloSetup-universal-26.10.20.dmg`
đã ghim từ VNG, đối chiếu SHA-256, giải nén và chỉ giữ `app.asar`, cài vào
`<userData>/ZaloRuntime/Contents/Resources/app.asar`. Repo không kèm code của VNG.

Ba cách khác để có runtime:

```bash
# 1. chỉ định sẵn app.asar (macOS: /Applications/Zalo.app/Contents/Resources/app.asar;
#    Windows: giải nén ZaloSetup.exe rồi lấy resources/app.asar)
OLAZ_ASAR=/đường/dẫn/app.asar cargo run --manifest-path linux/src-tauri/Cargo.toml

# 2. cài từ bộ cài bạn đã tải (DMG/EXE/ZIP — 7-Zip giải nén được tất cả)
OLAZ_INSTALLER=~/Downloads/ZaloSetup-universal-26.10.20.dmg \
  cargo run --manifest-path linux/src-tauri/Cargo.toml -- --fetch-runtime

# 3. tắt tự tải, tự lo runtime sau
OLAZ_AUTO_FETCH=0 cargo run --manifest-path linux/src-tauri/Cargo.toml
```

Khởi động không bao giờ chờ mạng: bước init chỉ đọc đĩa (bridge lên sau ~100 ms), chưa có runtime thì
cửa sổ mở ngay trang chẩn đoán nội bộ (`/__olaz/index.html`), rồi renderer được cài **ở nền** — mặc định
bật, tắt bằng `OLAZ_AUTO_FETCH=0`. Trang đó hiện kết quả kiểm tra khởi động (renderer đã cài chưa,
`7-Zip`, nguồn cài, tự cài bật/tắt), tiến trình tải, và hai nút **Tải và cài Zalo PC từ VNG** /
**Kiểm tra lại**; cài xong cửa sổ tự chuyển sang renderer, không phải khởi động lại. Trong lúc chờ trang
vẫn tự kiểm tra bridge: spec, `getPath`, `fs.stat`, electron-store, crypto, và một request mạng qua proxy.
Cửa sổ **luôn mở trang này trước** rồi tự sang renderer khi sẵn sàng; từ bất kỳ trang nào, **Ctrl+Shift+D**
quay lại trang chẩn đoán. Lỗi JS của renderer và các lời gọi bridge bị lỗi được ghi vào log host
(`[renderer] [page] …`), nên một webview kẹt vẫn để lại dấu vết thay vì chỉ hiện logo.

### Tự lấy runtime khác gì bản macOS

| | macOS (`ZaloRuntimeInstaller.swift`) | Linux (`runtime.rs`) |
|---|---|---|
| nguồn | DMG ghim, fallback `zalo.me/pc` | như vậy |
| mở gói | `hdiutil attach` | 7-Zip (`p7zip-full`) |
| tin cậy | chữ ký Apple của VNG (`SecStaticCodeCheckValidity`) | **SHA-256 của bản ghim** + kiểm tra `pc-dist/login.html` trong asar |
| kiểm tra sau khi giải nén | `isComplete` (plist + asar + ZaloCall) | asar phải mở được và có renderer; ghi `<root>/version` |

### Tự cập nhật runtime

Mỗi lần khởi động, sau khi thấy runtime đã cài, app hỏi `zalo.me/download/zalo-pc` xem bản mới nhất là gì
(chỉ `HEAD`, không tải). Nếu mới hơn bản đang dùng, app tải và cài vào `ZaloRuntime/pending` ở nền;
phiên đang mở vẫn dùng bản cũ. Lần chạy sau, `pending` được kiểm tra lại (asar mở được, có
`pc-dist/login.html`, version mới hơn) rồi mới thay `Contents/Resources/app.asar` — không ghi đè file
đang được phục vụ. Bản tải từ `zalo.me` không có SHA-256 để đối chiếu (giống nhánh fallback của lần cài
đầu), nên chỉ được nhận sau khi asar đọc được và có renderer; bản ghim vẫn là mặc định khi không có mạng.
Tắt bằng `OLAZ_AUTO_UPDATE=0`, hoặc `OLAZ_AUTO_FETCH=0` (tắt mọi tải runtime). `OLAZ_ASAR` và
`OLAZ_INSTALLER` cũng không bị tự cập nhật vì người dùng đã chỉ định nguồn.

Chữ ký Apple không kiểm tra được ngoài macOS, nên với bộ cài cục bộ hãy truyền
`OLAZ_INSTALLER_SHA256=<hex>` nếu bạn có mã đối chiếu; không có thì chỉ còn kiểm tra cấu trúc.
Nếu tải sai/thiếu, asar sẽ bị từ chối trước khi cài.

Gói 7-Zip cần: `sudo apt install p7zip-full`.

### TLS/HTTP-2 fingerprint

Bản macOS gửi mọi request qua `FingerprintHelper/` (Go, tls-client profile `Chrome_108`) để VNG thấy
đúng ClientHello của Zalo PC (Electron 22.3.9 / Chromium 108). Linux dùng **chính helper đó**, build
cho `linux/amd64` và bundle vào AppImage như sidecar — không có bản sao thứ hai của mã Go:

```bash
linux/scripts/build-fingerprint-helper.sh   # → src-tauri/binaries/olaz-fingerprint-helper-<triple>
```

`tauri.conf.json` khai báo `externalBin`, nên trong AppImage helper nằm cạnh binary (`usr/bin/`).
`src-tauri/src/fingerprint.rs` là bản Rust của `FingerprintProxy.swift`: spawn helper, nói chuyện qua
unix socket theo protocol trong `FingerprintHelper/protocol.go`, đưa request HTTP/WebSocket qua đó.
Helper không khởi động được thì request đi bằng rustls — đúng kiểu bản macOS rơi về URLSession.

Đo qua bridge bằng `https://tls.peet.ws/api/clean`:

| | JA3 | JA4 | HTTP |
|---|---|---|---|
| helper (`Chrome_108`) | `cd08e31494f9531f560d64c695473da9` | `t13d1516h2_8daaf6152771_e5627efa2ab1` | h2 |
| rustls (fallback) | `17cf038fbbd05178bd571345b652933b` | `t13d1011h1_61a7ad8aa9b6_3fcd1a44f3e3` | HTTP/1.1 |

WebSocket qua helper hiện không dùng được: preset utls cho websocket trong fork `utls` đang ghim trả
`please implement this method`, nên socket realtime rơi về tungstenite (rustls). Lúc đó socket chưa
mở nên không có việc đổi fingerprint giữa phiên.

### Biến môi trường

| biến | tác dụng |
|---|---|
| `OLAZ_ASAR` | đường dẫn `app.asar` (hoặc thư mục chứa nó) |
| `OLAZ_USERDATA` | thư mục dữ liệu (mặc định `$XDG_DATA_HOME/olaz`) |
| `OLAZ_CLIENT_VERSION` | ghi đè `client_version` khi không đọc được từ `main-dist/main.js` |
| `OLAZ_DEBUG=1` | log request mạng, state, các member/kênh chưa cài |
| `OLAZ_DEV_EVAL=1` | bật dev harness trong bản release; build debug mặc định bật (đặt `0` để tắt) |
| `OLAZ_AUTO_FETCH=0` | không tự tải runtime khi chưa có |
| `OLAZ_AUTO_UPDATE=0` | không tự tải bản runtime mới hơn (mặc định bật) |
| `OLAZ_INSTALLER` | cài từ bộ cài cục bộ thay vì tải (`.dmg`/`.exe`/`.zip`) |
| `OLAZ_INSTALLER_SHA256` | mã SHA-256 mong đợi của bộ cài cục bộ |
| `OLAZ_ALLOW_LATEST=0` | không fallback sang bản `zalo.me/pc` đang phát hành |
| `OLAZ_FINGERPRINT=0` | không dùng fingerprint helper (mọi request đi bằng rustls) |
| `OLAZ_FINGERPRINT_HELPER` | đường dẫn helper, nếu nó không nằm cạnh binary |
| `OLAZ_DUMP=1` | in 3000 ký tự đầu body của các endpoint `/api/login/*` và `qr-id.*` (xem server trả gì khi QR bị từ chối) |

`--serve-only` chạy mỗi loopback bridge, không mở cửa sổ (dùng để kiểm thử):

```bash
cargo run --manifest-path linux/src-tauri/Cargo.toml -- --serve-only
curl -s -X POST localhost:<port>/sync -d '{"path":"$zelectron.getPathSync","args":["userData"]}'
```

## Đóng gói AppImage

```bash
linux/scripts/build-appimage.sh
# → <cargo target>/release/bundle/appimage/Olaz_1.0.0_amd64.AppImage
```

Script chạy `cargo tauri build --bundles appimage` (cần `@tauri-apps/cli`; lần đầu Tauri tải
`linuxdeploy`/`AppRun` về). AppImage tự chứa binary + icon; `app.asar` vẫn do người dùng trỏ vào
bằng `OLAZ_ASAR` (hoặc đặt ở `<userData>/ZaloRuntime/Contents/Resources/app.asar`).

Nếu `CARGO_TARGET_DIR` được đặt (như môi trường build hiện tại, trỏ `/storage/cargo-target`), bundle
nằm ở đó chứ không phải `linux/src-tauri/target`. Bản đã build và kiểm tra nằm ở
`linux/dist/Olaz_1.0.0_amd64.AppImage` (81 MB, ELF x86-64).

Chạy thử không cần FUSE:

```bash
APPIMAGE_EXTRACT_AND_RUN=1 OLAZ_ASAR=/path/app.asar linux/dist/Olaz_1.0.0_amd64.AppImage
```

## Đã có gì / chưa có gì

Đã chạy:

* tự cập nhật runtime: kiểm tra bản mới trên `zalo.me` lúc khởi động, tải vào `pending` ở nền và
  chỉ chuyển sang ở lần chạy sau (xem mục “Tự cập nhật runtime”)
* đọc `app.asar`, phục vụ `pc-dist/**` qua loopback, ép `fail_sqlite3=true` như bản macOS
* **inject script**: `initialization_script` của Tauri *không chạy* với `WebviewUrl::External`
  trên WebKitGTK (kiểm chứng: cửa sổ không hề gọi `/poll`), nên bundle được phục vụ như
  `/__olaz/boot.js` và chèn vào đầu `<head>` của mọi trang HTML; CSP meta bị bỏ để script chạy —
  tương đương `WKUserScript` trên macOS, vốn cũng không bị CSP chặn
* bridge tự chứa `fetch` gốc: `rpc()` không được đi qua `fetch` mà `network-shim.js` đã thay,
  nếu không mọi lời gọi async sẽ đệ quy trở lại chính proxy
* bridge 450 thành viên IPC + các member `$znode`/`$zelectron`; member lạ trả `undefined` và ghi log
  (đúng hành vi fallback của `ZaloBridge.swift`, promise luôn settle nên renderer không treo)
* `local-modules.js` chạy nguyên vẹn: `path`, `os`, electron-store, `createHash`/`createCipheriv`
  có trạng thái (primitive nằm ở `bridge.rs`: MD5/SHA-1/256/384/512, AES-CBC, AES-GCM, HMAC, HKDF)
* fs sync + async (stat/mkdir/readdir/unlink/access/copy/remove/rename), `getPath` theo XDG
* `zsafe-storage` (AES-256-CBC + IV cố định như preload) và `zprotocol` (HKDF-SHA256 → AES-CBC + HMAC)
* cookie jar dùng chung giữa `setAppCookie` và proxy mạng; UA `ZaloPC-darwin-23v<version>`
* đăng nhập QR đã chạy với tài khoản thật. Cookie phiên (`zpw_sek`, …) được lưu ở
  `cookies.json` trong userdata và nạp lại khi khởi động: có phiên thì cửa sổ vào thẳng
  `index.html?…&already_login=y`, không có thì mở trang QR
* dung lượng ổ đĩa lấy từ `statvfs` thật qua `$znode.nativelibs.fileUtils().getDiskUsage` và
  `$znode.diskSpace` (trước đây chưa cài nên trả `undefined` → Zalo báo "đầy ổ cứng" dù còn trống)
* URL trang renderer mang `?type=23&client_version=<runtime>&app_version=…&fail_sqlite3=true` như
  main của Zalo PC: renderer đọc `apiType`/`apiVersion` từ query (thiếu thì rơi về localStorage rỗng),
  nên mở `login.html` trơn khiến `reqqr` đi với `type=null&client_version=null` — mã QR server cấp
  cho client không xác định và điện thoại báo “mã không hợp lệ”
* HTTP qua fingerprint helper (ClientHello Chromium 108, header Chromium 108); WebSocket rơi về tungstenite
  (`Origin: file://`, đọc có timeout 100 ms) vì preset utls cho WS trong helper trả
  `please implement this method` (xem mục fingerprint bên dưới)
* realtime: frame WS được đẩy sang trang với đúng tên `text`/`binary` như `network-shim.js` yêu cầu
  (trước đây gửi `message` nên mọi frame đến đều bị bỏ), upgrade mang cookie phiên như URLSession trên macOS,
  và mỗi socket có generation riêng — thread cũ không xoá nhầm socket mới khi id bắt đầu lại từ 1 sau khi đổi trang
* cookie cho WebSocket: cookie renderer đặt theo host (`zalo.me`, `chat.zalo.me`) được khớp thêm theo
  domain cha khi upgrade lên `ws1-msg.chat.zalo.me`. Thiếu cookie, server trả `HTTP/1.1 200 OK` dạng text;
  tungstenite báo `Reserved bits are non-zero`, renderer reconnect liên tục — sidebar vẫn có preview nhờ
  polling `get-last-msgs` nhưng chat detail không nhận tin realtime
* `OLAZ_DEBUG=1` in chẩn đoán frame realtime, **chỉ header** `v=<version> cmd=<u16 LE> sub=<byte> len=<n>`
  (text frame chỉ `len`), tối đa 80 frame mỗi socket, không bao giờ in nội dung — cùng mẫu `describeFrame`
  của `NetworkProxy` trên macOS
* `login-success` / `login` → điều hướng trang; `db-authenticated`, `db-client-connected` → push lại
  renderer; host → page qua long-poll `/poll`
* lưu trữ IndexedDB (WebKitGTK có sẵn), không cần port SQLCipher DAL
* origin ổn định: bridge bind cổng cố định **27182** (đổi bằng `OLAZ_PORT`) thay vì cổng ngẫu nhiên.
  WebKitGTK khoá localStorage/IndexedDB theo scheme+host+**cổng**, nên cổng đổi sau mỗi lần chạy
  làm Zalo mất DB tin nhắn (IndexedDB) và cache — tin nhắn không đồng bộ lại, hội thoại cũ biến mất.
  Khi 27182 bận, app dừng và báo rõ thay vì lặng lẽ sang cổng khác (origin khác ⇒ dữ liệu mới).

Chưa có (khác bản macOS):

* **Giao diện**: bản macOS vẽ UI SwiftUI và ẩn DOM của renderer. Bản Linux hiển thị chính DOM đó —
  đúng như Zalo PC, nhưng chưa có giao diện native.
* Điều khiển cửa sổ từ renderer (`minimize`, `toggle-maximize`, badge, notification) mới chỉ ack.
* `multipart/form-data` (upload ảnh/file) chưa dựng lại body; `fetch`/XHR dạng FormData sẽ lỗi.
* `shared-worker.html` chạy trong một cửa sổ Tauri ẩn cùng origin; bridge chuyển tiếp
  `shared-worker-*` (đăng ký task, run/resp/progress/abort, remote config) giữa hai cửa sổ và
  gửi `receive-viewerkey` cho worker, giống `SharedWorkerRouter` bên macOS.
* Không port `ZaloCall`/`ZaloCap` (thoại, chụp màn hình).
* WebSocket realtime đi bằng tungstenite (xem mục fingerprint ở trên).
* `$zFileManager` đã port theo bản macOS: `z_u.txt` (`getViewerkey` phải reject khi thiếu),
  log trong `<userData>/cal/`, `getFileArrayBuffer`/`readFileAsync`/`svcStatAsync`, đọc theo range
  `svcReadPath`, `writeBufferToPath`, `is_trust_path → true`; thêm `getFileBlob`/`getFileBlobURL`
  bằng Blob/object URL trong `linux/web/file-manager.js` để avatar và media cache đọc từ đĩa;
  `readChunksArrayObject` (đọc một part để upload, trả `DataView[]` + cập nhật `refHash`) cũng ở đó.
  `$zwindow.showOpenDialog`/`$zFileManager.getPhotoWithPaths` dùng `<input type=file>` của WebKitGTK
  rồi ghi bản sao vào `<userData>/olaz-uploads/`, vì renderer chỉ nhận đường dẫn cho upload file.
  Nhánh mặc định `$zpc.zfile.readFileByChunksForUpload`/`getChecksum` (Node stream) được route sang
  reader và `getMd5LargeFileObject` (md5 streaming trong Rust).
  `openDirectory` (chọn thư mục cho upload thư mục/backup) mở `gtk::FileChooserDialog` trên main
  context qua member `$zwindow.pickFolder`; `openFile` vẫn dùng `<input type=file>`.
  Chưa port: transform E2EE khi upload, de-noise `ZaloDownloads/fileNoise` (báo lỗi thay vì gửi sai),
  backup/zip.

### Developer harness (Linux)

Bản macOS có `Olaz --engine-probe` và dev channel đọc file trong `<data>/dev`; bản Linux port đầy đủ
các tool đó. Harness **bật mặc định trong build debug**; bản release chỉ mở khi đặt `OLAZ_DEV_EVAL=1`.
Đặt `OLAZ_DEV_EVAL=0` để tắt cả trong build debug:

```bash
# build debug: harness tự bật
DISPLAY=:0 OLAZ_DEBUG=1 cargo run --manifest-path linux/src-tauri/Cargo.toml

# AppImage/release: phải bật rõ ràng
DISPLAY=:0 OLAZ_DEBUG=1 OLAZ_DEV_EVAL=1 linux/dist/Olaz_1.0.0_amd64.AppImage
```

HTTP:

```bash
# Chạy trong main renderer (mặc định) hoặc shared-worker ẩn
curl -s -X POST localhost:27182/__olaz/dev/eval \
  -d '{"code":"return {title: document.title, text: document.body.innerText.slice(0, 300)}"}'
curl -s -X POST localhost:27182/__olaz/dev/eval \
  -d '{"target":"worker","code":"return 6*7"}'

# Fiber React, feature detection và DOM summary — tương đương --engine-probe
curl -s localhost:27182/__olaz/dev/probe
```

Kết quả nằm trong `result` (JSON string); endpoint trả 403 khi không bật và không ghi code/kết quả
vào log. `target` nhận `main` hoặc `worker`/`shared-worker`.

### Đồng bộ tin nhắn (Điện thoại → PC)

Luồng sync chính thức của Zalo chạy qua cloud: PC gọi sync → điện thoại xác
nhận trên app Zalo → dữ liệu kéo về qua `transfersync.zaloapp.com` và ghi vào
IndexedDB bởi DAL worker. Điều khiển từ loopback (không cần `OLAZ_DEV_EVAL`):

```bash
curl -s localhost:27182/__olaz/transfer          # trạng thái (enabled/running/canSync)
curl -s -X POST localhost:27182/__olaz/transfer/start   # bắt đầu sync (điện thoại sẽ nhận yêu cầu xác nhận)
curl -s -X POST localhost:27182/__olaz/transfer/cancel  # huỷ sync đang chạy
```

Trong UI chính thức, card "Đồng bộ tin nhắn" ở **Cài đặt** cũng dùng được như
bản macOS. Ảnh của tin mới import được photo warmer tự tải (theo dõi số dòng
store, chạy lại khi có ≥ 20 dòng mới).

File tools, giống bản macOS (thư mục `<userData>/dev`, mặc định `~/.local/share/olaz/dev`):

| file | tác dụng |
|---|---|
| `eval.js` | chạy trong main renderer khi file đổi; ghi `eval.out.json` |
| `worker-eval.js` | chạy trong shared-worker ẩn; ghi `worker-eval.out.json` |
| `open-conv.txt` | chứa conversation id; mở cuộc trò chuyện đó một lần rồi xoá file |
| `compose.json` | chạy một composer action; ghi `compose.out.txt` |

`compose.json` có dạng `{"method":"myCard","args":[],"files":[]}`. Với `files`, harness đọc
đường dẫn qua `$zFileManager`, tạo `File` và inject vào input ẩn mà composer adapter bấm — không mở
hộp thoại chọn file. `method:"screenshot"` trả `unsupported` vì `ZaloCap`/`ScreenCapture` chưa port sang
Linux. Dùng để kiểm tra DOM, store, chat detail và các action soạn tin khi không chụp được màn hình.

## Kiểm thử

```bash
cargo test --manifest-path linux/src-tauri/Cargo.toml
node linux/scripts/check-bridge.js <port>        # cần một instance --serve-only
```

`cargo test` không cần mạng, không cần `app.asar`: asar reader trên archive tự dựng, AES-CBC/GCM
roundtrip, SHA-256, HKDF, và runtime installer (đọc tên bộ cài, SHA-256, tìm `app.asar` trong cây
giải nén, so sánh phiên bản, chuyển bản chờ) — 19 test. `check-bridge.js` gọi đúng những lời gọi mà `HostScripts/*.js` thực hiện — sync
member, crypto, store, fs, zsafe, zproto, kênh lạ — và kiểm tra việc phục vụ `login.html` từ
`app.asar` (13 kiểm tra).

Installer kiểm được mà không cần tải 323 MB, dựng một zip cùng layout:

```bash
OLAZ_INSTALLER=ZaloSetup-universal-26.10.20.dmg OLAZ_INSTALLER_SHA256=<hex> \
  cargo run --manifest-path linux/src-tauri/Cargo.toml -- --fetch-runtime
```

## Giấy phép

GPL-3.0, như phần còn lại của repo. `app.asar` là mã của VNG, không nằm trong repo này.
