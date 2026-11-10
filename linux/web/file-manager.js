// Linux-only `$zFileManager` members that Electron's preload built in the page
// itself (`U3kX`), so Rust cannot return them: Blob/object URLs for avatars and
// cached media, the native picker, and the chunk reader used by uploads. Same
// shape: files are read through the synchronous bridge (loopback XHR, exactly
// the path `bridge.js` already uses for every `*Sync` member).
(() => {
  'use strict';
  const local = window.__zaloLocal || (window.__zaloLocal = {});
  const bridge = () => window.__zaloBridge;
  const read = (path) => bridge().callSync('$zFileManager.getFileArrayBuffer', [path]);

  // Both are async in the official preload (`Dt = async (e,t) =>
  // t.createObjectURL(new Blob([await Nt(e)]))`), and callers `await` them.
  local['$zFileManager.getFileBlob'] = async (path) => new Blob([read(path)]);
  local['$zFileManager.getFileBlobURL'] = async (path) =>
    URL.createObjectURL(new Blob([read(path)]));

  // ------------------------------------------------------------- file picker
  // macOS answers the hidden `<input type=file>` from NSOpenPanel; WebKitGTK
  // opens its own chooser for the same input, so the picker needs no native
  // code. `click()` runs in the Promise executor, i.e. still inside the toolbar
  // click's user gesture, which WebKit requires before opening a chooser.
  const pickFiles = (options = {}) => new Promise((resolve) => {
    const input = document.createElement('input');
    input.type = 'file';
    input.multiple = !!options.multiple;
    if (options.accept) input.accept = options.accept;
    input.style.display = 'none';
    document.body.appendChild(input);
    let settled = false;
    let focusTimer;
    let timeout;
    const finish = (files) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      clearTimeout(focusTimer);
      window.removeEventListener('focus', onFocus);
      input.remove();
      resolve(files);
    };
    // Newer WebKit fires `cancel`; older versions only give the window focus
    // back, so both paths end the wait (the fallback wins 1 s later).
    const onFocus = () => {
      clearTimeout(focusTimer);
      focusTimer = setTimeout(() => {
        finish(input.files ? Array.from(input.files) : []);
      }, 1000);
    };
    timeout = setTimeout(() => finish([]), 5 * 60 * 1000);
    input.addEventListener('change', () => finish(Array.from(input.files || [])));
    input.addEventListener('cancel', () => finish([]));
    window.addEventListener('focus', onFocus);
    input.click();
  });

  const readFileBytes = (file) => new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(new Uint8Array(reader.result));
    reader.onerror = () => reject(reader.error || new Error('read failed'));
    reader.readAsArrayBuffer(file);
  });

  // The renderer only accepts paths for file uploads (`stat`, checksum and the
  // chunk reader all reopen the path), so picked bytes become a real file under
  // <userData>/olaz-uploads. The basename survives: the UI displays it.
  const writePicked = (name, bytes) => {
    const root = String(window.__zaloCwd || '').replace(/\/+$/, '');
    const dir = root + '/olaz-uploads/' + Date.now().toString(36) + '-' +
      Math.random().toString(36).slice(2, 8);
    const safe = String(name || 'file').replace(/[^\w.\- ()]+/g, '_').slice(-160) || 'file';
    const target = dir + '/' + safe;
    bridge().callSync('$zFileManager.writeBufferToPath', [target, bytes]);
    return target;
  };

  // `$zwindow.showOpenDialog` (used by "Tệp" and by `getPhotoWithPaths`).
  // Directories have no browser equivalent (an `<input webkitdirectory>` never
  // reveals the absolute path), so they go to the native GTK chooser.
  local['$zwindow.showOpenDialog'] = async (options = {}) => {
    const properties = Array.isArray(options.properties) ? options.properties : [];
    if (properties.includes('openDirectory')) {
      const folder = await bridge().callAsync('$zwindow.pickFolder', [
        String(options.title || ''),
        String(options.defaultPath || ''),
      ]);
      return folder
        ? { canceled: false, filePaths: [folder] }
        : { canceled: true, filePaths: [] };
    }
    if (!properties.includes('openFile')) return { canceled: true, filePaths: [] };
    const accept = (Array.isArray(options.filters) ? options.filters : [])
      .flatMap((filter) => (filter && Array.isArray(filter.extensions) ? filter.extensions : []))
      .filter((ext) => ext && ext !== '*')
      .map((ext) => '.' + String(ext).toLowerCase())
      .join(',');
    const files = await pickFiles({ multiple: properties.includes('multiSelections'), accept });
    if (!files.length) return { canceled: true, filePaths: [] };
    const filePaths = [];
    for (const file of files) filePaths.push(writePicked(file.name, await readFileBytes(file)));
    return { canceled: false, filePaths };
  };

  // Official preload shape (`I` in `U3kX`): every selected path is reported,
  // small photos carry their bytes, big ones only `needReadBlob`.
  local['$zFileManager.getPhotoWithPaths'] = async (title, defaultPath, options = {}) => {
    const extensions = (options.extensions || ['jpg', 'jpeg', 'png', 'gif'])
      .map((ext) => String(ext).toLowerCase());
    const files = await pickFiles({
      multiple: true,
      accept: extensions.map((ext) => '.' + ext).join(','),
    });
    if (!files.length) throw new Error("can't get photoPaths!");
    const photoPaths = [];
    const photoResults = [];
    for (const file of files) {
      const bytes = await readFileBytes(file);
      const path = writePicked(file.name, bytes);
      photoPaths.push(path);
      const ext = (file.name.split('.').pop() || '').toLowerCase();
      if (!extensions.includes(ext)) {
        photoResults.push({
          isSuccess: false,
          data: { errCode: 0, message: `file doesn't have valid extension for photo! - ${ext}` },
        });
        continue;
      }
      const readable = !options.maxSizePhoto || bytes.byteLength <= options.maxSizePhoto;
      photoResults.push({
        isSuccess: true,
        data: readable
          ? { name: file.name, path, size: bytes.byteLength, content: bytes, needReadBlob: false }
          : { name: file.name, path, size: bytes.byteLength, needReadBlob: true },
      });
    }
    return { photoPaths, photoResults };
  };

  // `readChunksArrayObject(key, fileInfo, chunkSize, part, secureOptions)` —
  // the official preload (`cn`) streams one byte window of a path-backed file,
  // slices it into `DataView`s and feeds the caller's incremental hash. Here the
  // window comes from the same `svcReadPath` range read the media cache uses;
  // it is one upload part, so buffering it in memory matches the official
  // stream, which also collects the whole part before slicing.
  // The official stream additionally pipes E2EE transforms when `secureOptions`
  // is set; those live in the preload's crypto module and are not ported here,
  // so a secure read is refused instead of uploading plaintext under a tag.
  local['$zFileManager.readChunksArrayObject'] = async (
    key, fileInfo, chunkSize, part, secureOptions,
  ) => {
    if (!fileInfo || typeof fileInfo !== 'object') throw new Error('invalid file');
    if (!Number.isInteger(chunkSize) || chunkSize <= 0) throw new Error('invalid chunk size');
    if (secureOptions) throw new Error('E2EE chunk read is not supported on Linux');
    // Official preload: folder/file objects from the native picker carry the
    // real path in `folderPath`/`filePath`; every other object reads `path`.
    const path = (fileInfo.subType != null && (fileInfo.folderPath || fileInfo.filePath)) ||
      fileInfo.path;
    if (!path) throw new Error('path is not trust');
    // Official stream XOR-de-noises forwarded files under ZaloDownloads/fileNoise.
    if (path.includes('ZaloDownloads') && path.includes('fileNoise')) {
      throw new Error('fileNoise de-noise is not supported on Linux');
    }

    const size = Number(fileInfo.size) || 0;
    const noChunk = Math.ceil(size / chunkSize);
    const start = Math.max(1, (part && part.startChunkId) || 1);
    const end = Math.min(noChunk, (part && part.endChunkId) || noChunk);
    const refHash = part && part.refHash;
    const chunks = [];
    if (end >= start) {
      const startByte = (start - 1) * chunkSize;
      const endByte = Math.min(size, end * chunkSize);
      const expected = endByte - startByte;
      let bytes = bridge().callSync('$zFileManager.svcReadPath', [
        path, { start: startByte, end: endByte },
      ]);
      if (!(bytes instanceof Uint8Array)) bytes = new Uint8Array(bytes);
      if (bytes.byteLength !== expected) {   // the official stream pads short reads
        const padded = new Uint8Array(expected);
        padded.set(bytes.subarray(0, expected));
        bytes = padded;
      }
      for (let id = start; id <= end; id += 1) {
        const relative = (id - start) * chunkSize;
        const length = Math.min(chunkSize, expected - relative);
        if (length <= 0) break;
        const offset = bytes.byteOffset + relative;
        chunks.push(new DataView(bytes.buffer, offset, length));
        if (refHash && refHash.hash && typeof refHash.hash.append === 'function') {
          refHash.hash.append(bytes.buffer.slice(offset, offset + length));
          refHash.size = (Number(refHash.size) || 0) + length;
        }
      }
    }
    return { chunks, noChunk };
  };

  // ------------------------------------------------------- `$zpc.zfile` route
  // With `async_forward.enable_de_noise_in_forward` (on by default) the renderer
  // reads path-backed uploads through `$zpc.zfile`, whose Node-stream version
  // needs `$znode.fs.createReadStream` (not implemented here). `$zpc` is built
  // by the renderer bundle, so intercept its assignment and answer both calls
  // from the bridge implementations above.
  const fileMd5 = (path) => {
    const target = String(path);
    if (target.includes('ZaloDownloads') && target.includes('fileNoise')) {
      return Promise.reject(new Error('fileNoise de-noise is not supported on Linux'));
    }
    return bridge().callAsync('$zFileManager.getMd5LargeFileObject', [target]);
  };
  const routeZpc = (zpc) => {
    const zfile = zpc && zpc.zfile;
    if (!zfile || zfile.__olazUploadBridge) return zpc;
    const define = (name, value) => {
      try {
        Object.defineProperty(zfile, name, { configurable: true, writable: true, value });
      } catch {}
    };
    define('readFileByChunksForUpload', (key, fileInfo, options) => {
      const part = (options && options.part) || {};
      const hash = (options && options.hash) || {};
      return local['$zFileManager.readChunksArrayObject'](
        key,
        fileInfo,
        part.chunkSize,
        {
          startChunkId: part.startChunkId,
          endChunkId: part.endChunkId,
          refHash: { hash: hash.ref, last: hash.last, size: hash.size },
        },
        options && options.secure,
        part.isLastPart,
      );
    });
    define('getChecksum', (path) => fileMd5(path));
    define('compareChecksum', async (path, expected) => expected === await fileMd5(path));
    try {
      Object.defineProperty(zfile, '__olazUploadBridge', { value: true });
    } catch {}
    return zpc;
  };
  let zpc = routeZpc(window.$zpc);
  try {
    Object.defineProperty(window, '$zpc', {
      configurable: true,
      enumerable: true,
      get: () => zpc,
      set: (value) => { zpc = routeZpc(value); },
    });
  } catch {}
})();
