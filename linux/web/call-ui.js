// Olaz call UI (Linux): picker thành viên cuộc gọi nhóm + banner cuộc gọi đến,
// vẽ ngay trong trang Zalo (bản Linux không có lớp UI native như macOS).
//
// Nguồn sự kiện:
//   * host (`host.rs` `handle_call_state`) đẩy control `kind:"js"` gọi
//     `window.__olazCallUI.openPicker(...)` / `.closePicker(...)` khi
//     `call-adapter.js` báo `{kind:'picker'}` / `{kind:'pickerClosed'}` qua zstate.
//   * `{kind:'incoming'}` hiện banner (host cũng đã báo notification desktop).
// Trả lời: `window.__zaloCall.pickerConfirm(token, ids)` / `.pickerCancel(token)`
// (được `call-adapter.js` cài, chạy đúng `onConfirm` / `onCancel` của modal gốc).
//
// Modal GROUP_CALL gốc của renderer bị adapter nuốt (trang không render nó),
// nên panel này thay đúng chỗ đó — hết "click chết" cho cuộc gọi nhóm.
(() => {
  'use strict';
  if (window.__olazCallUI) return;
  if (location.pathname.split('/').pop() !== 'index.html') return;

  const HOST = document.head || document.documentElement;

  // ------------------------------------------------------------------ styles
  const style = document.createElement('style');
  style.id = '__olaz-call-ui-style';
  style.textContent = `
    #olaz-call-overlay { position: fixed; inset: 0; z-index: 2147483000;
      background: rgba(0,0,0,.45); display: flex; align-items: center; justify-content: center; }
    #olaz-call-overlay[hidden] { display: none; }
    .olaz-call-panel { width: 400px; max-width: calc(100vw - 48px); max-height: min(560px, calc(100vh - 64px));
      background: #fff; border-radius: 12px; display: flex; flex-direction: column; overflow: hidden;
      box-shadow: 0 12px 48px rgba(0,0,0,.35); font-family: inherit; }
    .olaz-call-head { padding: 16px 20px 12px; border-bottom: 1px solid #eee; display: flex; gap: 12px; align-items: center; }
    .olaz-call-head img, .olaz-call-head .olaz-call-fallback { width: 40px; height: 40px; border-radius: 50%; object-fit: cover; }
    .olaz-call-fallback { background: #0091ff; color: #fff; display: flex; align-items: center; justify-content: center;
      font-weight: 600; flex: none; }
    .olaz-call-title { font-size: 15px; font-weight: 600; color: #111; }
    .olaz-call-sub { font-size: 12px; color: #757575; margin-top: 2px; }
    .olaz-call-note { margin: 10px 20px 0; padding: 8px 12px; background: #fff7e6; color: #8a6116;
      border-radius: 8px; font-size: 12px; }
    .olaz-call-list { flex: 1; overflow-y: auto; padding: 8px 8px 4px; margin: 6px 0; }
    .olaz-call-row { display: flex; align-items: center; gap: 12px; padding: 8px 12px; border-radius: 8px;
      cursor: pointer; user-select: none; }
    .olaz-call-row:hover { background: #f5f5f5; }
    .olaz-call-row.disabled { cursor: default; opacity: .55; }
    .olaz-call-row img, .olaz-call-row .olaz-call-fallback { width: 36px; height: 36px; border-radius: 50%; object-fit: cover; flex: none; }
    .olaz-call-row .olaz-call-name { flex: 1; font-size: 14px; color: #111;
      overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .olaz-call-row input { width: 18px; height: 18px; accent-color: #0091ff; flex: none; }
    .olaz-call-foot { padding: 12px 20px 16px; display: flex; align-items: center; gap: 12px; border-top: 1px solid #eee; }
    .olaz-call-count { flex: 1; font-size: 13px; color: #757575; }
    .olaz-call-btn { padding: 8px 20px; border-radius: 8px; border: none; font-size: 14px; cursor: pointer; }
    .olaz-call-btn.primary { background: #0091ff; color: #fff; }
    .olaz-call-btn.primary:disabled { background: #b9d9f5; cursor: default; }
    .olaz-call-btn.plain { background: #f0f0f0; color: #333; }
    .olaz-call-banner { position: fixed; top: 10px; left: 50%; transform: translateX(-50%); z-index: 2147483000;
      width: 460px; max-width: calc(100vw - 32px); background: #1f1f1f; color: #fff; border-radius: 20px;
      padding: 10px 14px; display: flex; align-items: center; gap: 12px; box-shadow: 0 8px 32px rgba(0,0,0,.4);
      animation: olaz-call-in .25s ease-out; }
    .olaz-call-banner[hidden] { display: none; }
    @keyframes olaz-call-in { from { transform: translate(-50%, -12px); opacity: 0; } to { transform: translate(-50%, 0); opacity: 1; } }
    .olaz-call-banner img, .olaz-call-banner .olaz-call-fallback { width: 38px; height: 38px; border-radius: 50%; object-fit: cover; }
    .olaz-call-banner .olaz-call-info { flex: 1; min-width: 0; }
    .olaz-call-label { font-size: 11.5px; color: #b5b5b5; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .olaz-call-banner .olaz-call-title { color: #fff; font-size: 14px; font-weight: 600; margin: 1px 0;
      overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .olaz-call-banner .olaz-call-open { background: #0091ff; color: #fff; border: none; border-radius: 8px;
      padding: 7px 14px; font-size: 13px; cursor: pointer; }
    .olaz-call-banner .olaz-call-hide { background: none; border: none; color: #9e9e9e; font-size: 16px;
      cursor: pointer; padding: 4px 6px; line-height: 1; }
    .olaz-call-banner .olaz-call-hide:hover { color: #fff; }
    .olaz-call-toast { position: fixed; left: 50%; bottom: 72px; transform: translateX(-50%); z-index: 2147483001;
      background: rgba(20,20,20,.92); color: #fff; padding: 9px 18px; border-radius: 20px; font-size: 13px;
      animation: olaz-call-in .15s ease-out; }
  `;
  HOST.appendChild(style);

  // ------------------------------------------------------------------ helpers
  let toastTimer = null;
  const toast = (message) => {
    document.querySelectorAll('.olaz-call-toast').forEach((node) => node.remove());
    const node = document.createElement('div');
    node.className = 'olaz-call-toast';
    node.textContent = message;
    document.body.appendChild(node);
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => node.remove(), 2400);
  };

  const avatarNode = (src, name) => {
    const fallback = document.createElement('div');
    fallback.className = 'olaz-call-fallback';
    fallback.textContent = (name || '?').trim().charAt(0).toUpperCase() || '?';
    if (!src) return fallback;
    const img = document.createElement('img');
    img.src = src;
    img.alt = '';
    img.addEventListener('error', () => img.replaceWith(fallback));
    return img;
  };

  const RESULT_TEXT = {
    started: 'Đã bắt đầu cuộc gọi',
    sent: 'Đã thêm vào cuộc gọi',
    stale: 'Yêu cầu đã cũ',
    invalid: 'Chưa chọn hợp lệ',
    busy: 'Đang trong một cuộc gọi khác',
    offline: 'Máy đang ngoại tuyến',
    unsupported: 'Máy không hỗ trợ gọi',
    'not-ready': 'Chưa sẵn sàng',
    'not-open': 'Chưa mở được hội thoại',
    unavailable: 'Không thể gọi',
    error: 'Không thực hiện được',
    cancelled: 'Đã huỷ',
  };

  // ------------------------------------------------------------------ picker
  let currentToken = null;
  let overlay = null;
  let selected = new Set();
  let pickerMax = 10;

  const closePanel = () => {
    currentToken = null;
    selected = new Set();
    overlay?.remove();
    overlay = null;
  };

  const renderRow = (member) => {
    const existing = (member.__existing);
    const row = document.createElement('div');
    row.className = 'olaz-call-row' + (existing ? ' disabled' : '');
    row.appendChild(avatarNode(member.avatar, member.name));
    const name = document.createElement('div');
    name.className = 'olaz-call-name';
    name.textContent = member.name || member.id;
    name.title = member.name || member.id;
    row.appendChild(name);
    const box = document.createElement('input');
    box.type = 'checkbox';
    box.checked = existing || selected.has(member.id);
    box.disabled = existing;
    const toggle = () => {
      if (existing) return;
      if (selected.has(member.id)) {
        selected.delete(member.id);
        box.checked = false;
      } else {
        if (selected.size >= pickerMax) {
          toast(`Tối đa ${pickerMax} thành viên cho cuộc gọi này`);
          return;
        }
        selected.add(member.id);
        box.checked = true;
      }
      updateCount();
    };
    row.addEventListener('click', toggle);
    row.appendChild(box);
    return row;
  };

  let countNode = null;
  let confirmNode = null;
  const updateCount = () => {
    if (countNode) countNode.textContent = `Đã chọn ${selected.size}/${pickerMax}`;
    if (confirmNode) confirmNode.disabled = selected.size === 0;
  };

  const openPicker = (payload) => {
    const data = payload && typeof payload === 'object' ? payload : {};
    const members = Array.isArray(data.members) ? data.members : [];
    const existing = Array.isArray(data.existing) ? data.existing.map(String) : [];
    closePanel();
    currentToken = String(data.token || '');
    pickerMax = Number(data.max) || 10;
    selected = new Set();

    overlay = document.createElement('div');
    overlay.id = 'olaz-call-overlay';
    const panel = document.createElement('div');
    panel.className = 'olaz-call-panel';

    const head = document.createElement('div');
    head.className = 'olaz-call-head';
    const group = data.group || {};
    head.appendChild(avatarNode(group.avatar, group.name));
    const headText = document.createElement('div');
    headText.style.minWidth = '0';
    const title = document.createElement('div');
    title.className = 'olaz-call-title';
    title.textContent = data.add ? 'Thêm vào cuộc gọi' : (data.video ? 'Cuộc gọi video nhóm' : 'Cuộc gọi nhóm');
    const sub = document.createElement('div');
    sub.className = 'olaz-call-sub';
    sub.textContent = group.name || '';
    sub.title = group.name || '';
    headText.appendChild(title);
    headText.appendChild(sub);
    head.appendChild(headText);
    panel.appendChild(head);

    if (data.note) {
      const note = document.createElement('div');
      note.className = 'olaz-call-note';
      note.textContent = data.note;
      panel.appendChild(note);
    }

    const list = document.createElement('div');
    list.className = 'olaz-call-list';
    for (const member of members) {
      member.__existing = existing.includes(String(member.id));
      list.appendChild(renderRow(member));
    }
    if (!members.length) {
      const empty = document.createElement('div');
      empty.className = 'olaz-call-sub';
      empty.style.padding = '16px 20px';
      empty.textContent = 'Không có thành viên nào để chọn.';
      list.appendChild(empty);
    }
    panel.appendChild(list);

    const foot = document.createElement('div');
    foot.className = 'olaz-call-foot';
    countNode = document.createElement('div');
    countNode.className = 'olaz-call-count';
    foot.appendChild(countNode);
    const cancel = document.createElement('button');
    cancel.className = 'olaz-call-btn plain';
    cancel.textContent = 'Huỷ';
    cancel.addEventListener('click', () => {
      const token = currentToken;
      closePanel();
      try { window.__zaloCall?.pickerCancel(token); } catch {}
    });
    foot.appendChild(cancel);
    confirmNode = document.createElement('button');
    confirmNode.className = 'olaz-call-btn primary';
    confirmNode.textContent = data.add ? 'Thêm' : 'Gọi';
    confirmNode.addEventListener('click', () => {
      const token = currentToken;
      const ids = [...selected];
      closePanel();
      try {
        Promise.resolve(window.__zaloCall?.pickerConfirm(token, ids))
          .then((result) => toast(RESULT_TEXT[result] || `Kết quả: ${result}`))
          .catch(() => toast('Không thực hiện được'));
      } catch {}
    });
    foot.appendChild(confirmNode);
    panel.appendChild(foot);

    overlay.appendChild(panel);
    document.body.appendChild(overlay);
    updateCount();
  };

  const closePicker = (token) => {
    if (token == null || String(token) === currentToken) closePanel();
  };

  // ------------------------------------------------------------------ banner
  let banner = null;
  let bannerTimer = null;
  const hideBanner = () => {
    clearTimeout(bannerTimer);
    banner?.remove();
    banner = null;
  };
  const showBanner = (payload) => {
    const data = payload && typeof payload === 'object' ? payload : {};
    hideBanner();
    const group = !!data.group;
    // Như `ZaloEngine+CallHook`: nhóm được báo bằng tên nhóm, fallback tên
    // người gọi rồi tới chữ chung.
    const name = data.name || data.caller || (group ? 'Cuộc gọi nhóm' : 'Một người bạn');
    banner = document.createElement('div');
    banner.className = 'olaz-call-banner';
    banner.appendChild(avatarNode(data.avatar, name));
    const info = document.createElement('div');
    info.className = 'olaz-call-info';
    const label = document.createElement('div');
    label.className = 'olaz-call-label';
    label.textContent = group ? 'Cuộc gọi nhóm đến' : 'Cuộc gọi đến';
    info.appendChild(label);
    const title = document.createElement('div');
    title.className = 'olaz-call-title';
    title.textContent = name;
    title.title = name;
    info.appendChild(title);
    // Nhóm: biết tên nhóm rồi mới kèm ai là người gọi (như banner macOS).
    if (group && data.name && data.caller) {
      const caller = document.createElement('div');
      caller.className = 'olaz-call-label';
      caller.textContent = `${data.caller} đang gọi`;
      info.appendChild(caller);
    }
    banner.appendChild(info);
    // zrtc engine mode: nhận máy thật (402 + media). Ở virtual mode channel
    // này không có ai xử lý nên nút là no-op vô hại.
    const accept = document.createElement('button');
    accept.className = 'olaz-call-open';
    accept.style.background = '#2e9e4f';
    accept.textContent = 'Trả lời';
    accept.addEventListener('click', () => {
      hideBanner();
      try {
        window.webkit?.messageHandlers?.zbridge?.postMessage({ kind: 'send', channel: 'call-accept', args: [] });
      } catch {}
    });
    banner.appendChild(accept);
    // Helper nội bộ (virtual mode) có thể từ chối thật: gửi signal 405/12437
    // qua bridge — renderer relay lên server như helper macOS đã làm.
    const decline = document.createElement('button');
    decline.className = 'olaz-call-open';
    decline.style.background = '#555';
    decline.textContent = 'Từ chối';
    decline.addEventListener('click', () => {
      hideBanner();
      try {
        window.webkit?.messageHandlers?.zbridge?.postMessage({ kind: 'send', channel: 'call-decline', args: [] });
      } catch {}
    });
    banner.appendChild(decline);
    const open = document.createElement('button');
    open.className = 'olaz-call-open';
    open.textContent = 'Mở Zalo';
    open.addEventListener('click', () => {
      hideBanner();
      try {
        window.webkit?.messageHandlers?.zbridge?.postMessage({ kind: 'send', channel: 'focus-window', args: [] });
      } catch {}
    });
    banner.appendChild(open);
    // Nút "Ẩn" của banner macOS.
    const hide = document.createElement('button');
    hide.className = 'olaz-call-hide';
    hide.textContent = '✕';
    hide.title = 'Ẩn';
    hide.addEventListener('click', hideBanner);
    banner.appendChild(hide);
    document.body.appendChild(banner);
    bannerTimer = setTimeout(hideBanner, 45_000);
  };

  window.__olazCallUI = { openPicker, closePicker, showBanner, hideBanner, toast };

  // ------------------------------------------------------------------ in-call panel
  // zrtc engine mode: trạng thái cuộc gọi đi qua channel `call-ui`
  // (callState {state} và UiStatus {status}); renderer nhận call-update
  // riêng, panel này chỉ là lớp hiển thị.
  let callPanel = null;
  let callTimerInterval = null;
  let callConnectedAt = null;

  const fmtDuration = (ms) => {
    const s = Math.floor(ms / 1000);
    const mm = String(Math.floor(s / 60)).padStart(2, '0');
    const ss = String(s % 60).padStart(2, '0');
    return `${mm}:${ss}`;
  };

  const hideCallPanel = () => {
    clearInterval(callTimerInterval);
    callTimerInterval = null;
    callConnectedAt = null;
    callPanel?.remove();
    callPanel = null;
  };

  const showCallPanel = (statusText) => {
    if (!callPanel) {
      callPanel = document.createElement('div');
      callPanel.className = 'olaz-call-banner';
      callPanel.style.top = 'auto';
      callPanel.style.bottom = '18px';
      const info = document.createElement('div');
      info.className = 'olaz-call-info';
      const status = document.createElement('div');
      status.className = 'olaz-call-title';
      status.textContent = statusText || '';
      info.appendChild(status);
      const clock = document.createElement('div');
      clock.className = 'olaz-call-label';
      info.appendChild(clock);
      callPanel.appendChild(info);
      const end = document.createElement('button');
      end.className = 'olaz-call-open';
      end.style.background = '#d64545';
      end.textContent = 'Kết thúc';
      end.addEventListener('click', () => {
        try {
          window.webkit?.messageHandlers?.zbridge?.postMessage({ kind: 'send', channel: 'call-control', args: [{ action: 'end' }] });
        } catch {}
        hideCallPanel();
      });
      callPanel.appendChild(end);
      document.body.appendChild(callPanel);
    }
    const status = callPanel.querySelector('.olaz-call-title');
    if (status && statusText) status.textContent = statusText;
  };

  const startClock = () => {
    if (callTimerInterval) return;
    callConnectedAt = Date.now();
    callTimerInterval = setInterval(() => {
      const clock = callPanel?.querySelector('.olaz-call-label');
      if (clock && callConnectedAt) clock.textContent = fmtDuration(Date.now() - callConnectedAt);
    }, 1000);
  };

  const onCallUi = (data) => {
    if (!data || typeof data !== 'object') return;
    if (data.state === 'free') return hideCallPanel();
    if (data.state === 'incall') return; // banner đã hiển thị từ renderer
    if (typeof data.status === 'string') {
      const text = {
        connecting: 'Đang kết nối…',
        connected: 'Đã kết nối',
        serverUnreachable: 'Không tới được máy chủ',
        callFailed: 'Cuộc gọi lỗi',
        recipientDeclined: 'Người nhận từ chối',
        recipientUnreachable: 'Người nhận không khả dụng',
        recipientNoAnswer: 'Không ai bắt máy',
        ringing: 'Đang reo…',
      }[data.status] || data.status;
      showCallPanel(text);
      if (data.status === 'connected') startClock();
    }
  };

  // Bọc ipcEmit để nghe channel `call-ui` (sự kiện host → page đi qua đây).
  const hookEmit = () => {
    const bridge = window.__zaloBridge;
    if (!bridge || typeof bridge.ipcEmit !== 'function' || bridge.__olazCallUiHooked) return;
    const original = bridge.ipcEmit.bind(bridge);
    bridge.ipcEmit = (channel, args) => {
      try {
        if (channel === 'call-ui') onCallUi(args?.[0]);
      } catch {}
      return original(channel, args);
    };
    bridge.__olazCallUiHooked = true;
  };
  hookEmit();
  setTimeout(hookEmit, 1500);
})();
