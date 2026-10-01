function syncBrowserTimezone() {
  const timezone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  if (!timezone) return;
  const readTimezone = () => {
    const cookie = document.cookie.split(';').map(value => value.trim())
      .find(value => value.startsWith('journey_timezone='));
    if (!cookie) return null;
    try {
      return decodeURIComponent(cookie.slice('journey_timezone='.length));
    } catch (_) {
      return null;
    }
  };

  if (readTimezone() !== timezone) {
    document.cookie = `journey_timezone=${timezone}; Path=/; SameSite=Lax`;
    if (readTimezone() === timezone) window.location.reload();
  }

  formatLocalTimes(document);
}

function formatLocalTimes(root) {
  root.querySelectorAll('time[data-local-time][datetime]').forEach(element => {
    const instant = new Date(element.dateTime);
    if (!Number.isNaN(instant.getTime())) {
      element.textContent = new Intl.DateTimeFormat(undefined, {
        dateStyle: 'medium',
        timeStyle: 'short',
      }).format(instant);
    }
  });
}

syncBrowserTimezone();

const initializedVideoPreviews = new WeakSet();
let galleryPreviewObserver = null;
let galleryVisibilityCheckScheduled = false;
let galleryFallbackListenersAttached = false;

function videoHasCurrentFrame(video) {
  return video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA && video.videoWidth > 0;
}

function initializeVideoPreview(video, onFrameReady = () => {}, onFrameError = () => {}) {
  if (initializedVideoPreviews.has(video)) return;
  initializedVideoPreviews.add(video);

  let userStarted = false;
  let userSeeked = false;
  let previewSeekRequested = false;

  function revealFrameIfReady() {
    if (!videoHasCurrentFrame(video)) return false;
    video.dataset.previewReady = 'true';
    onFrameReady();
    return true;
  }

  function prepareFrame() {
    if (revealFrameIfReady() || userStarted || userSeeked || !video.paused || previewSeekRequested) return;
    if (video.readyState < HTMLMediaElement.HAVE_METADATA || !Number.isFinite(video.duration) || video.duration <= 0) return;

    const previewTime = Math.min(0.1, video.duration / 2);
    if (Math.abs(video.currentTime - previewTime) < 0.001) return;
    previewSeekRequested = true;
    try {
      video.currentTime = previewTime;
    } catch (_) {
      previewSeekRequested = false;
    }
  }

  video.addEventListener('play', () => { userStarted = true; });
  video.addEventListener('seeking', () => {
    if (!previewSeekRequested) userSeeked = true;
  });
  video.addEventListener('loadstart', () => {
    video.removeAttribute('data-preview-ready');
    previewSeekRequested = false;
    onFrameError();
  });
  video.addEventListener('loadedmetadata', prepareFrame);
  video.addEventListener('loadeddata', revealFrameIfReady);
  video.addEventListener('seeked', revealFrameIfReady);
  video.addEventListener('error', () => {
    video.removeAttribute('data-preview-ready');
    onFrameError();
  });

  if (video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA) revealFrameIfReady();
  else if (video.readyState >= HTMLMediaElement.HAVE_METADATA) prepareFrame();
}

function initializeVideoPreviews(root = document) {
  if (root.matches?.('video[data-video-preview]')) initializeVideoPreview(root);
  root.querySelectorAll?.('video[data-video-preview]').forEach(video => initializeVideoPreview(video));
}

function loadGalleryVideoPreview(video) {
  if (video.hasAttribute('data-preview-requested')) return;
  const source = video.closest('.gallery-item')?.dataset.mediaSrc;
  if (!source) return;

  video.dataset.previewRequested = 'true';
  const frame = video.closest('[data-video-preview-frame]');
  initializeVideoPreview(
    video,
    () => frame?.setAttribute('data-ready', 'true'),
    () => frame?.removeAttribute('data-ready'),
  );
  video.preload = 'metadata';
  video.src = source;
  video.load();
}

function checkGalleryVideoVisibility() {
  galleryVisibilityCheckScheduled = false;
  const margin = 800;
  document.querySelectorAll('video[data-gallery-video-preview]:not([data-preview-requested])').forEach(video => {
    const bounds = video.getBoundingClientRect();
    if (bounds.bottom >= -margin && bounds.top <= window.innerHeight + margin) {
      loadGalleryVideoPreview(video);
    }
  });
}

function scheduleGalleryVideoVisibilityCheck() {
  if (galleryVisibilityCheckScheduled) return;
  galleryVisibilityCheckScheduled = true;
  window.requestAnimationFrame(checkGalleryVideoVisibility);
}

function initializeGalleryVideoPreviews(root = document) {
  const videos = [];
  if (root.matches?.('video[data-gallery-video-preview]')) videos.push(root);
  videos.push(...root.querySelectorAll?.('video[data-gallery-video-preview]') || []);
  if (videos.length === 0) return;

  if ('IntersectionObserver' in window) {
    if (!galleryPreviewObserver) {
      galleryPreviewObserver = new IntersectionObserver(entries => {
        entries.forEach(entry => {
          if (!entry.isIntersecting) return;
          galleryPreviewObserver.unobserve(entry.target);
          loadGalleryVideoPreview(entry.target);
        });
      }, { rootMargin: '800px 0px' });
    }
    videos.forEach(video => galleryPreviewObserver.observe(video));
    return;
  }

  if (!galleryFallbackListenersAttached) {
    window.addEventListener('scroll', scheduleGalleryVideoVisibilityCheck, { passive: true });
    window.addEventListener('resize', scheduleGalleryVideoVisibilityCheck, { passive: true });
    galleryFallbackListenersAttached = true;
  }
  scheduleGalleryVideoVisibilityCheck();
}

initializeVideoPreviews();
initializeGalleryVideoPreviews();

const sidebarLayout = document.querySelector('#site-layout');

function redirectToLogin() {
  const loginUrl = new URL('/login', window.location.origin);
  loginUrl.searchParams.set('return_to', window.location.pathname + window.location.search);
  window.location.assign(loginUrl);
}

if (sidebarLayout) {
  const sidebar = document.querySelector('#site-sidebar');
  const toggle = document.querySelector('#sidebar-toggle');
  const backdrop = document.querySelector('#sidebar-backdrop');
  const pageContent = sidebarLayout.querySelector('.site');
  let sidebarOpen = false;

  function updateSidebarState(open, restoreFocus = false) {
    sidebarOpen = open;
    sidebarLayout.classList.toggle('sidebar-open', open);
    sidebar.hidden = !open;
    toggle.setAttribute('aria-expanded', String(open));
    toggle.setAttribute('aria-label', open ? 'Close sidebar' : 'Open sidebar');
    toggle.firstElementChild.textContent = open ? '‹' : '›';
    backdrop.hidden = !open;
    document.body.classList.toggle('sidebar-modal-open', open);
    pageContent.inert = open;
    if (restoreFocus) toggle.focus({ preventScroll: true });
  }

  toggle.addEventListener('click', () => updateSidebarState(!sidebarOpen));
  backdrop.addEventListener('click', () => updateSidebarState(false, true));
  document.addEventListener('keydown', event => {
    if (event.key === 'Escape' && sidebarOpen) {
      event.preventDefault();
      updateSidebarState(false, true);
      return;
    }
    if (event.key !== 'Tab' || !sidebarOpen) return;
    const focusable = [toggle, ...sidebar.querySelectorAll('a[href], button:not([disabled])')]
      .filter(element => !element.closest('[hidden]'));
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    } else if (!sidebar.contains(document.activeElement) && document.activeElement !== toggle) {
      event.preventDefault();
      first.focus();
    }
  });
  document.querySelectorAll('[data-sidebar-expand]').forEach(button => {
    const list = document.getElementById(button.dataset.sidebarExpand);
    if (!list) return;
    const overflowItems = list.querySelectorAll('[data-overflow-item="true"]');
    if (overflowItems.length === 0) return;
    overflowItems.forEach(item => { item.hidden = true; });
    button.hidden = false;
    button.addEventListener('click', () => {
      const expanded = button.getAttribute('aria-expanded') !== 'true';
      overflowItems.forEach(item => { item.hidden = !expanded; });
      button.setAttribute('aria-expanded', String(expanded));
      button.textContent = expanded ? button.dataset.collapseLabel : button.dataset.expandLabel;
    });
  });
}

const createdConfirmation = document.querySelector('[data-created-confirmation]');

if (createdConfirmation) {
  const currentUrl = new URL(window.location.href);
  currentUrl.searchParams.delete('created');
  window.history.replaceState(window.history.state, '', currentUrl);
  window.setTimeout(() => createdConfirmation.remove(), 5000);
}

const publishDialog = document.querySelector('#publish-dialog');

if (publishDialog) {
  const closeButton = publishDialog.querySelector('.publish-dialog-close');
  const cancelButton = document.querySelector('#publish-cancel');
  const publishForm = document.querySelector('#publish-form');
  const useTimeCheckbox = document.querySelector('#publish-use-time');
  const overrideArea = document.querySelector('#publish-time-override');
  const timeInput = document.querySelector('#publish-time');
  const confirmation = document.querySelector('#publish-confirmation');
  const errorMessage = document.querySelector('#publish-error');
  const submitButton = document.querySelector('#publish-submit');
  let activePublishButton = null;
  let currentPostId = null;
  let publishBusy = false;

  function currentLocalMinute() {
    const now = new Date();
    return new Date(now.getTime() - now.getTimezoneOffset() * 60000)
      .toISOString().slice(0, 16);
  }

  function clearPublishError() {
    errorMessage.hidden = true;
    errorMessage.textContent = '';
  }

  function updatePublishConfirmation() {
    overrideArea.hidden = !useTimeCheckbox.checked;
    timeInput.required = useTimeCheckbox.checked;
    timeInput.removeAttribute('aria-invalid');
    clearPublishError();

    if (!useTimeCheckbox.checked) {
      confirmation.textContent = 'The post will be published now using the server time.';
      submitButton.textContent = 'Publish now';
      return true;
    }

    const instant = new Date(timeInput.value);
    if (!timeInput.value || Number.isNaN(instant.getTime())) {
      confirmation.textContent = 'Choose a valid local date and time to continue.';
      submitButton.textContent = 'Publish at this time';
      return false;
    }
    confirmation.textContent = `The post will be published at ${new Intl.DateTimeFormat(undefined, {
      dateStyle: 'full',
      timeStyle: 'short',
    }).format(instant)}.`;
    submitButton.textContent = 'Publish at this time';
    return true;
  }

  function setPublishBusy(busy) {
    publishBusy = busy;
    publishForm.querySelectorAll('input, button').forEach(control => {
      control.disabled = busy;
    });
    closeButton.disabled = busy;
    publishForm.setAttribute('aria-busy', String(busy));
  }

  document.addEventListener('click', event => {
    if (!(event.target instanceof Element)) return;
    const button = event.target.closest('[data-publish-post]');
    if (!button) return;
    activePublishButton = button;
    currentPostId = button.dataset.publishPost;
    publishForm.reset();
    timeInput.value = currentLocalMinute();
    clearPublishError();
    updatePublishConfirmation();
    publishDialog.showModal();
    closeButton.focus({ preventScroll: true });
  });

  useTimeCheckbox.addEventListener('change', updatePublishConfirmation);
  timeInput.addEventListener('input', updatePublishConfirmation);
  cancelButton.addEventListener('click', () => publishDialog.close());
  closeButton.addEventListener('click', () => publishDialog.close());
  publishDialog.addEventListener('click', event => {
    if (event.target === publishDialog && !publishBusy) publishDialog.close();
  });

  publishForm.addEventListener('submit', async event => {
    event.preventDefault();
    if (publishBusy || !currentPostId) return;
    clearPublishError();
    if (!updatePublishConfirmation()) {
      timeInput.setAttribute('aria-invalid', 'true');
      timeInput.focus();
      return;
    }

    if (draftForm && !document.querySelector('#draft-title').value.trim()) {
      errorMessage.textContent = 'Add a title before publishing this post.';
      errorMessage.hidden = false;
      return;
    }
    if (typeof window.journeySaveDraft === 'function') {
      try {
        await window.journeySaveDraft();
      } catch (_) {
        errorMessage.textContent = 'Save the latest draft changes before publishing.';
        errorMessage.hidden = false;
        return;
      }
    }

    let payload = {};
    if (useTimeCheckbox.checked) {
      const instant = new Date(timeInput.value);
      const publishedAt = Math.floor(instant.getTime() / 1000);
      if (!Number.isSafeInteger(publishedAt)) {
        timeInput.setAttribute('aria-invalid', 'true');
        errorMessage.textContent = 'Choose a valid date and time.';
        errorMessage.hidden = false;
        timeInput.focus();
        return;
      }
      payload = { published_at: publishedAt };
    }

    const postId = currentPostId;
    setPublishBusy(true);
    try {
      const response = await fetch(`/api/posts/${encodeURIComponent(postId)}/publish`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload),
      });
      if (response.status === 401) {
        redirectToLogin();
        return;
      }
      if (response.status === 204) {
        window.location.assign(`/posts/${encodeURIComponent(postId)}`);
        return;
      }
      if (response.status === 403) {
        errorMessage.textContent = 'This account is not allowed to publish this post.';
      } else if (response.status === 404) {
        errorMessage.textContent = 'This draft is no longer available to publish.';
      } else if (response.status === 409) {
        errorMessage.textContent = 'This post has already been published.';
      } else {
        const returnedError = (await response.text()).trim();
        errorMessage.textContent = returnedError.includes('title')
          ? 'Add a title before publishing this post.'
          : returnedError || 'Could not publish this post. Try again.';
      }
      errorMessage.hidden = false;
    } catch (_) {
      errorMessage.textContent = 'Could not publish this post. Check your connection and try again.';
      errorMessage.hidden = false;
    } finally {
      setPublishBusy(false);
      if (publishDialog.open) submitButton.focus({ preventScroll: true });
    }
  });

  publishDialog.addEventListener('close', () => {
    currentPostId = null;
    clearPublishError();
    if (activePublishButton?.isConnected) activePublishButton.focus({ preventScroll: true });
    activePublishButton = null;
  });
}

const draftForm = document.querySelector('#draft-form');

if (draftForm) {
  const rootBlockList = document.querySelector('#draft-root-blocks');
  const titleInput = document.querySelector('#draft-title');
  const summaryInput = document.querySelector('#draft-summary');
  const tagsInput = document.querySelector('#draft-tags');
  const submitButton = document.querySelector('#draft-submit');
  const errorMessage = document.querySelector('#draft-form-error');
  const statusMessage = document.querySelector('#draft-status');
  const publishButton = document.querySelector('#draft-publish');
  const publishHelp = document.querySelector('#draft-publish-help');
  let postId = Number(draftForm.dataset.draftPostId) || null;
  let revision = null;
  let editVersion = 0;
  let dirty = false;
  let saveTimer = null;
  let operationQueue = Promise.resolve();
  const uploadedFiles = new WeakMap();
  let draggedMedia = null;

  function clearDraftError() {
    errorMessage.hidden = true;
    errorMessage.textContent = '';
  }

  function showDraftError(message) {
    errorMessage.textContent = message;
    errorMessage.hidden = false;
  }

  function queueOperation(operation) {
    const result = operationQueue.then(operation);
    operationQueue = result.catch(() => {});
    return result;
  }

  function updatePublishButton() {
    publishButton.hidden = !postId;
    publishButton.dataset.publishPost = postId ? String(postId) : '';
    const titleIsBlank = !titleInput.value.trim();
    publishButton.disabled = titleIsBlank;
    publishHelp.hidden = !postId || !titleIsBlank;
  }

  function updateStatus(message) {
    statusMessage.textContent = message;
  }

  function makeActionButton(label, action, className = '') {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = `draft-small-button ${className}`.trim();
    button.dataset.blockAction = action;
    button.textContent = label;
    return button;
  }

  function makeMediaActionButton(label, action, className = '') {
    const button = makeActionButton(label, action, className);
    delete button.dataset.blockAction;
    button.dataset.mediaAction = action;
    return button;
  }

  function makeBlockField(labelText, fieldName, multiline) {
    const label = document.createElement('label');
    label.className = 'draft-block-field';
    const caption = document.createElement('span');
    caption.textContent = labelText;
    const field = document.createElement(multiline ? 'textarea' : 'input');
    field.dataset.blockField = fieldName;
    if (multiline) {
      field.rows = 5;
    } else {
      field.type = 'text';
    }
    label.append(caption, field);
    return label;
  }

  function makeDraftBlock() {
    const block = document.createElement('fieldset');
    block.className = 'draft-block';
    block.dataset.draftBlock = '';

    const legend = document.createElement('legend');
    legend.textContent = 'Block';
    block.append(legend);

    const actions = document.createElement('div');
    actions.className = 'draft-block-actions';
    actions.append(
      makeActionButton('Move up', 'move-up'),
      makeActionButton('Move down', 'move-down'),
      makeActionButton('Remove block', 'remove', 'draft-remove-button'),
    );
    block.append(actions);

    const fields = document.createElement('div');
    fields.className = 'draft-block-fields';
    fields.append(makeBlockField('Header (optional)', 'header', false));
    fields.append(makeBlockField('Body (optional)', 'body', true));
    block.append(fields);

    const gallery = document.createElement('section');
    gallery.className = 'draft-gallery';
    const galleryHeading = document.createElement('h3');
    galleryHeading.textContent = 'Gallery';
    const galleryList = document.createElement('div');
    galleryList.className = 'draft-gallery-list';
    galleryList.dataset.galleryList = '';
    const fileInput = document.createElement('input');
    fileInput.className = 'draft-file-input';
    fileInput.type = 'file';
    fileInput.multiple = true;
    fileInput.accept = 'image/jpeg,image/png,image/webp,image/gif,image/heic,image/heif,video/mp4,video/quicktime,.jpg,.jpeg,.png,.webp,.gif,.heic,.heif,.mp4,.mov';
    const addMedia = makeActionButton('Add media', 'select-media');
    const dropHelp = document.createElement('p');
    dropHelp.className = 'draft-drop-help';
    dropHelp.textContent = 'Drop files here or choose several. Drag a thumbnail to move it between blocks.';
    gallery.append(galleryHeading, galleryList, fileInput, addMedia, dropHelp);
    block.append(gallery);

    return block;
  }

  function makeMediaField(labelText, name, multiline = false) {
    const label = makeBlockField(labelText, name, multiline);
    label.classList.add('draft-media-field');
    return label;
  }

  function makeMediaItem(media = {}, pendingFileName = null) {
    const item = document.createElement('article');
    item.className = 'draft-media-item';
    item.dataset.draftMedia = '';
    item.draggable = !pendingFileName;
    if (pendingFileName) item.dataset.uploadPending = '';
    if (media.id) item.dataset.blockId = String(media.id);
    if (media.storage_key) item.dataset.storageKey = media.storage_key;
    if (media.content_type) item.dataset.contentType = media.content_type;
    if (media.alt) item.dataset.alt = media.alt;
    if (media.previewUrl) item.dataset.previewUrl = media.previewUrl;

    const preview = document.createElement('div');
    preview.className = 'draft-media-preview';
    const source = media.previewUrl || (postId && media.id
      ? `/posts/${encodeURIComponent(postId)}/blocks/${encodeURIComponent(media.id)}/media`
      : '');
    if (source && String(media.content_type || '').startsWith('image/')) {
      const image = document.createElement('img');
      image.src = source;
      image.alt = media.alt || '';
      preview.append(image);
    } else if (source && String(media.content_type || '').startsWith('video/')) {
      const video = document.createElement('video');
      video.preload = 'metadata';
      video.controls = true;
      video.dataset.videoPreview = '';
      initializeVideoPreview(video);
      video.src = source;
      preview.append(video);
    } else {
      preview.textContent = 'Original media';
    }
    if (pendingFileName) {
      const upload = document.createElement('div');
      upload.className = 'draft-media-upload';
      const name = document.createElement('strong');
      name.className = 'draft-media-upload-name';
      name.textContent = pendingFileName;
      const progress = document.createElement('progress');
      progress.className = 'draft-media-upload-progress';
      progress.max = 100;
      progress.value = 0;
      progress.setAttribute('aria-label', `Uploading ${pendingFileName}`);
      const detail = document.createElement('span');
      detail.className = 'draft-media-upload-detail';
      detail.textContent = 'Waiting to upload…';
      upload.append(name, progress, detail);
      preview.append(upload);
    }

    const fields = document.createElement('div');
    fields.className = 'draft-media-fields';
    const labelField = makeMediaField('Label (optional)', 'header');
    const captionField = makeMediaField('Caption (optional)', 'body', true);
    const altField = makeMediaField('Alt text (optional)', 'alt');
    labelField.querySelector('[data-block-field="header"]').value = media.header || '';
    captionField.querySelector('[data-block-field="body"]').value = media.body || '';
    altField.querySelector('[data-block-field="alt"]').value = media.alt || '';
    fields.append(labelField, captionField, altField);

    const actions = document.createElement('div');
    actions.className = 'draft-media-actions';
    actions.append(
      makeMediaActionButton('Duplicate', 'duplicate-media'),
      makeMediaActionButton('Remove media', 'remove-media', 'draft-remove-button'),
    );
    if (pendingFileName) {
      actions.querySelectorAll('button').forEach(button => { button.disabled = true; });
    }
    if (postId && media.id) {
      const download = document.createElement('a');
      download.className = 'media-download';
      download.href = `/posts/${encodeURIComponent(postId)}/blocks/${encodeURIComponent(media.id)}/media?download=1`;
      download.textContent = 'Download original';
      actions.append(download);
    }
    item.append(preview, fields, actions);
    return item;
  }

  function updateDraftBlockControls() {
    document.querySelectorAll('[data-block-list="root"]').forEach(list => {
      const blocks = Array.from(list.children).filter(child => child.matches('[data-draft-block]'));
      blocks.forEach((block, index) => {
        const legend = block.querySelector(':scope > legend');
        const actions = block.querySelector(':scope > .draft-block-actions');
        const label = `Block ${index + 1}`;
        legend.textContent = label;
        const moveUp = actions.querySelector('[data-block-action="move-up"]');
        const moveDown = actions.querySelector('[data-block-action="move-down"]');
        moveUp.disabled = index === 0;
        moveDown.disabled = index === blocks.length - 1;
        moveUp.setAttribute('aria-label', `Move ${label.toLowerCase()} up`);
        moveDown.setAttribute('aria-label', `Move ${label.toLowerCase()} down`);
        actions.querySelector('[data-block-action="remove"]').setAttribute('aria-label', `Remove ${label.toLowerCase()}`);
      });
    });
  }

  function blockFieldValue(block, name) {
    const field = block.querySelector(`[data-block-field="${name}"]`);
    return field.value.trim() ? field.value : null;
  }

  function blockId(node) {
    const id = Number(node.dataset.blockId);
    return Number.isSafeInteger(id) && id > 0 ? id : undefined;
  }

  function serializeBlock(block) {
    const mediaList = block.querySelector(':scope > .draft-gallery > [data-gallery-list]');
    const children = Array.from(mediaList.children)
      .filter(child => child.matches('[data-draft-media]') && !child.hasAttribute('data-upload-pending'))
      .map(item => {
        const child = {
          header: blockFieldValue(item, 'header'),
          body: blockFieldValue(item, 'body'),
          storage_key: item.dataset.storageKey || null,
          content_type: item.dataset.contentType || null,
          alt: blockFieldValue(item, 'alt'),
          children: [],
        };
        const id = blockId(item);
        if (id) child.id = id;
        return child;
      });
    const serialized = {
      header: blockFieldValue(block, 'header'),
      body: blockFieldValue(block, 'body'),
      children,
    };
    const id = blockId(block);
    if (id) serialized.id = id;
    return serialized;
  }

  function snapshotEditor() {
    const rootNodes = Array.from(rootBlockList.children).filter(block => block.matches('[data-draft-block]'));
    const blocks = rootNodes.map(serializeBlock);
    const mediaNodes = rootNodes.map(root => Array.from(root.querySelector('[data-gallery-list]').children)
      .filter(child => child.matches('[data-draft-media]') && !child.hasAttribute('data-upload-pending')));
    return {
      version: editVersion,
      rootNodes,
      mediaNodes,
      payload: {
        title: titleInput.value,
        summary: summaryInput.value,
        tags: tagsInput.value.split(',').map(tag => tag.trim()).filter(Boolean),
        blocks,
      },
    };
  }

  function applySavedIds(post, snapshot) {
    post.blocks.forEach((root, index) => {
      const rootNode = snapshot.rootNodes[index];
      if (rootNode) rootNode.dataset.blockId = String(root.id);
      (root.children || []).forEach((child, childIndex) => {
        const mediaNode = snapshot.mediaNodes[index]?.[childIndex];
        if (!mediaNode) return;
        mediaNode.dataset.blockId = String(child.id);
        if (child.storage_key) mediaNode.dataset.storageKey = child.storage_key;
        if (child.content_type) mediaNode.dataset.contentType = child.content_type;
        const download = mediaNode.querySelector('.media-download');
        if (!download) {
          const link = document.createElement('a');
          link.className = 'media-download';
          link.textContent = 'Download original';
          mediaNode.querySelector('.draft-media-actions').append(link);
        }
        const savedLink = mediaNode.querySelector('.media-download');
        savedLink.href = `/posts/${encodeURIComponent(postId)}/blocks/${encodeURIComponent(child.id)}/media?download=1`;
        const preview = mediaNode.querySelector('.draft-media-preview');
        const mediaUrl = `/posts/${encodeURIComponent(postId)}/blocks/${encodeURIComponent(child.id)}/media`;
        const image = preview.querySelector('img');
        const video = preview.querySelector('video');
        if (image) image.src = mediaUrl;
        if (video) video.src = mediaUrl;
      });
    });
  }

  async function saveDraftNow(force = false) {
    if (saveTimer) window.clearTimeout(saveTimer);
    saveTimer = null;
    if (postId && !dirty && !force) return true;
    clearDraftError();
    const snapshot = snapshotEditor();
    const payload = { ...snapshot.payload };
    const url = postId ? `/api/posts/${encodeURIComponent(postId)}` : '/api/posts';
    const method = postId ? 'PUT' : 'POST';
    if (postId) payload.revision = revision;
    submitButton.disabled = true;
    submitButton.textContent = 'Saving…';
    draftForm.setAttribute('aria-busy', 'true');
    updateStatus('Saving…');
    try {
      const response = await fetch(url, {
        method,
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload),
      });
      if (response.status === 409) {
        throw new Error('This draft changed in another session. Reload the page before saving again.');
      }
      if (!response.ok) {
        if (response.status === 401) throw new Error('Your session is no longer active. Sign in again before retrying.');
        if (response.status === 403) throw new Error('This account is not allowed to edit this draft.');
        if (response.status === 404) throw new Error('This draft is no longer available.');
        throw new Error('Could not save the draft. Your entries are still here; please try again.');
      }
      const saved = await response.json();
      if (!saved || !Number.isInteger(saved.id) || !Number.isInteger(saved.revision)) {
        throw new Error('The server could not confirm the saved draft. Your entries are still here; please retry.');
      }
      const wasNew = !postId;
      postId = saved.id;
      revision = saved.revision;
      draftForm.dataset.draftPostId = String(postId);
      if (wasNew) window.history.replaceState(null, '', `/posts/${encodeURIComponent(postId)}`);
      applySavedIds(saved, snapshot);
      updatePublishButton();
      if (editVersion === snapshot.version) {
        dirty = false;
        updateStatus(`Saved at ${new Date().toLocaleTimeString()}`);
      } else {
        updateStatus('New changes need saving…');
        scheduleSave();
      }
      return true;
    } catch (error) {
      const message = error instanceof Error ? error.message : 'Could not save the draft. Your entries are still here; please try again.';
      showDraftError(message);
      updateStatus('Save failed');
      throw error;
    } finally {
      submitButton.disabled = false;
      submitButton.textContent = 'Save draft';
      draftForm.removeAttribute('aria-busy');
    }
  }

  function scheduleSave() {
    if (saveTimer) window.clearTimeout(saveTimer);
    saveTimer = window.setTimeout(() => {
      saveTimer = null;
      queueOperation(() => saveDraftNow()).catch(() => {});
    }, 800);
  }

  function markDirty() {
    editVersion += 1;
    dirty = true;
    clearDraftError();
    updateStatus('Unsaved changes');
    scheduleSave();
  }

  function contentTypeForFile(file) {
    const accepted = new Set(['image/jpeg', 'image/png', 'image/webp', 'image/gif', 'image/heic', 'image/heif', 'video/mp4', 'video/quicktime']);
    const declared = (file.type || '').toLowerCase();
    if (accepted.has(declared)) return declared;
    const extension = file.name.split('.').pop().toLowerCase();
    return ({
      jpg: 'image/jpeg', jpeg: 'image/jpeg', png: 'image/png', webp: 'image/webp', gif: 'image/gif',
      heic: 'image/heic', heif: 'image/heif', mp4: 'video/mp4', mov: 'video/quicktime',
    })[extension] || null;
  }

  function makePendingMediaItem(file) {
    const previewUrl = URL.createObjectURL(file);
    const element = makeMediaItem({ content_type: contentTypeForFile(file), previewUrl }, file.name);
    return {
      element,
      previewUrl,
      progress: element.querySelector('.draft-media-upload-progress'),
      detail: element.querySelector('.draft-media-upload-detail'),
    };
  }

  function updateUploadProgress(upload, loaded, total) {
    if (!upload) return;
    if (total > 0) {
      const percent = Math.min(100, Math.round((loaded / total) * 100));
      upload.progress.value = percent;
      upload.detail.textContent = percent >= 100 ? 'Finishing upload…' : `${percent}% uploaded`;
    } else {
      upload.detail.textContent = 'Uploading…';
    }
  }

  function sendUploadRequest(url, file, contentType, onProgress) {
    return new Promise((resolve, reject) => {
      const request = new XMLHttpRequest();
      request.open('POST', url);
      request.setRequestHeader('Content-Type', contentType);
      request.upload.addEventListener('progress', event => {
        onProgress(event.loaded, event.lengthComputable ? event.total : file.size);
      });
      request.addEventListener('load', () => resolve({ status: request.status, text: request.responseText }));
      request.addEventListener('error', () => reject(new Error('the connection was interrupted')));
      request.addEventListener('abort', () => reject(new Error('the upload was canceled')));
      request.send(file);
    });
  }

  async function uploadFile(file, targetBlockId, onProgress, onRetry) {
    const contentType = contentTypeForFile(file);
    if (!contentType) throw new Error(`Unsupported media type: ${file.name}`);
    const pooled = uploadedFiles.get(file);
    if (pooled) {
      const uploaded = await pooled;
      onProgress(file.size, file.size);
      return uploaded;
    }
    const upload = (async () => {
      const url = `/posts/${encodeURIComponent(postId)}/blocks/${encodeURIComponent(targetBlockId)}/media`;
      let response = null;
      for (let attempt = 0; attempt < 2; attempt += 1) {
        try {
          response = await sendUploadRequest(url, file, contentType, onProgress);
        } catch (error) {
          if (attempt === 0) {
            onRetry();
            continue;
          }
          throw new Error(`Upload connection failed for ${file.name}: ${error instanceof Error ? error.message : 'check the connection and retry.'}`);
        }
        if (response.status === 502 && attempt === 0) {
          onRetry();
          continue;
        }
        break;
      }
      if (response.status === 413) throw new Error(`${file.name} is larger than the configured per-file upload limit.`);
      if (response.status < 200 || response.status >= 300) throw new Error(`Could not upload ${file.name} (HTTP ${response.status}). Check the connection and retry.`);
      let uploaded;
      try {
        uploaded = JSON.parse(response.text);
      } catch (_) {
        throw new Error(`The server could not confirm the upload of ${file.name}.`);
      }
      if (!uploaded || !uploaded.storage_key || uploaded.content_type !== contentType) {
        throw new Error(`The server could not confirm the upload of ${file.name}.`);
      }
      return {
        storage_key: uploaded.storage_key,
        content_type: uploaded.content_type,
      };
    })();
    uploadedFiles.set(file, upload);
    upload.catch(() => uploadedFiles.delete(file));
    return upload;
  }

  function discardPendingUpload(upload) {
    if (!upload.element.hasAttribute('data-upload-pending')) return;
    upload.element.removeAttribute('data-upload-pending');
    upload.element.remove();
    URL.revokeObjectURL(upload.previewUrl);
  }

  function queueMediaUpload(root, files) {
    if (!root || !root.isConnected || files.length === 0) return;
    const pendingUploads = files.map(file => makePendingMediaItem(file));
    const list = root.querySelector('[data-gallery-list]');
    pendingUploads.forEach(upload => list.append(upload.element));
    queueOperation(() => addFilesToBlock(root, files, pendingUploads)).catch(error => {
      showDraftError(error instanceof Error ? error.message : 'Media upload failed.');
      updateStatus('Upload failed');
    });
  }

  async function addFilesToBlock(root, files, pendingUploads) {
    if (!root.isConnected || files.length === 0) {
      pendingUploads.forEach(discardPendingUpload);
      return;
    }
    const failures = [];
    try {
      updateStatus('Preparing media upload…');
      await saveDraftNow();
      if (!root.isConnected) return;
      const targetBlockId = Number(root.dataset.blockId);
      if (!postId || !Number.isSafeInteger(targetBlockId) || targetBlockId < 1) {
        throw new Error('Save this block before uploading media to it.');
      }
      for (let index = 0; index < files.length; index += 1) {
        if (!root.isConnected) return;
        const file = files[index];
        const pending = pendingUploads[index];
        pending.detail.textContent = `Uploading ${index + 1} of ${files.length}…`;
        updateStatus(`Uploading ${index + 1} of ${files.length}: ${file.name}`);
        let uploaded;
        try {
          uploaded = await uploadFile(
            file,
            targetBlockId,
            (loaded, total) => updateUploadProgress(pending, loaded, total),
            () => {
              pending.progress.value = 0;
              pending.detail.textContent = 'Connection interrupted; retrying from the beginning…';
              updateStatus(`Connection interrupted; retrying ${file.name}…`);
            },
          );
        } catch (error) {
          const message = error instanceof Error ? error.message : 'upload failed';
          failures.push(`${file.name}: ${message}`);
          discardPendingUpload(pending);
          updateStatus(`Could not upload ${file.name}; continuing with the next file…`);
          continue;
        }
        if (!root.isConnected) return;
        pending.element.dataset.storageKey = uploaded.storage_key;
        pending.element.dataset.contentType = uploaded.content_type;
        pending.element.removeAttribute('data-upload-pending');
        pending.element.draggable = true;
        pending.element.querySelector('.draft-media-upload').remove();
        pending.element.querySelectorAll('[data-media-action]').forEach(button => { button.disabled = false; });
        markDirty();
        await saveDraftNow();
      }
      if (failures.length > 0) {
        throw new Error(`${failures.length} file${failures.length === 1 ? '' : 's'} failed to upload: ${failures.join('; ')}`);
      }
    } finally {
      pendingUploads.forEach(discardPendingUpload);
    }
  }

  function loadBlock(root, block) {
    root.dataset.blockId = String(block.id);
    root.querySelector('[data-block-field="header"]').value = block.header || '';
    root.querySelector('[data-block-field="body"]').value = block.body || '';
    const list = root.querySelector('[data-gallery-list]');
    (block.children || []).forEach(media => list.append(makeMediaItem(media)));
  }

  async function loadExistingDraft() {
    if (!postId) {
      updatePublishButton();
      updateStatus('Not saved yet.');
      return;
    }
    updateStatus('Loading draft…');
    rootBlockList.setAttribute('aria-busy', 'true');
    const controls = Array.from(draftForm.querySelectorAll('input, textarea, button'));
    controls.forEach(control => { control.disabled = true; });
    try {
      const response = await fetch(`/api/posts/${encodeURIComponent(postId)}`);
      if (!response.ok) throw new Error('Could not load this draft. Reload the page or check your access.');
      const post = await response.json();
      revision = post.revision;
      titleInput.value = post.title || '';
      summaryInput.value = post.summary || '';
      tagsInput.value = (post.tags || []).join(', ');
      (post.blocks || []).forEach(block => {
        const root = makeDraftBlock();
        loadBlock(root, block);
        rootBlockList.append(root);
      });
      updateDraftBlockControls();
      updatePublishButton();
      updateStatus('All changes saved');
    } catch (error) {
      showDraftError(error instanceof Error ? error.message : 'Could not load this draft.');
      updateStatus('Draft could not be loaded');
    } finally {
      controls.forEach(control => { control.disabled = false; });
      rootBlockList.removeAttribute('aria-busy');
    }
  }

  draftForm.addEventListener('click', event => {
    if (!(event.target instanceof Element)) return;
    const button = event.target.closest('[data-block-action]');
    if (!button) return;
    const action = button.dataset.blockAction;

    if (action === 'add-root') {
      const block = makeDraftBlock();
      rootBlockList.append(block);
      updateDraftBlockControls();
      block.querySelector('[data-block-field="header"]').focus();
      markDirty();
      return;
    }

    if (action === 'select-media') {
      button.closest('[data-draft-block]')?.querySelector('.draft-file-input').click();
      return;
    }

    const block = button.closest('[data-draft-block]');
    const list = block?.parentElement;
    if (!block || !list) return;
    const siblings = Array.from(list.children).filter(child => child.matches('[data-draft-block]'));
    const index = siblings.indexOf(block);
    if (action === 'remove') {
      const nextFocus = siblings[index + 1] || siblings[index - 1];
      block.remove();
      updateDraftBlockControls();
      (nextFocus?.querySelector('[data-block-field="header"]') || document.querySelector('[data-block-action="add-root"]')).focus();
      markDirty();
    } else if (action === 'move-up' && index > 0) {
      list.insertBefore(block, siblings[index - 1]);
      updateDraftBlockControls();
      button.focus();
      markDirty();
    } else if (action === 'move-down' && index < siblings.length - 1) {
      list.insertBefore(siblings[index + 1], block);
      updateDraftBlockControls();
      button.focus();
      markDirty();
    }
  });

  draftForm.addEventListener('click', event => {
    if (!(event.target instanceof Element)) return;
    const button = event.target.closest('[data-media-action]');
    if (!button) return;
    const item = button.closest('[data-draft-media]');
    if (!item) return;
    if (button.dataset.mediaAction === 'remove-media') {
      item.remove();
      markDirty();
    } else if (button.dataset.mediaAction === 'duplicate-media') {
      const copy = makeMediaItem({
        storage_key: item.dataset.storageKey,
        content_type: item.dataset.contentType,
        alt: blockFieldValue(item, 'alt'),
        header: blockFieldValue(item, 'header'),
        body: blockFieldValue(item, 'body'),
        previewUrl: item.dataset.previewUrl,
      });
      item.after(copy);
      markDirty();
    }
  });

  draftForm.addEventListener('input', event => {
    if (event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement) {
      if (event.target === titleInput) updatePublishButton();
      markDirty();
    }
  });

  draftForm.addEventListener('change', event => {
    if (!(event.target instanceof HTMLInputElement) || event.target.type !== 'file') return;
    const root = event.target.closest('[data-draft-block]');
    const files = Array.from(event.target.files || []);
    event.target.value = '';
    queueMediaUpload(root, files);
  });

  rootBlockList.addEventListener('dragstart', event => {
    const media = event.target instanceof Element ? event.target.closest('[data-draft-media]') : null;
    if (!media || media.hasAttribute('data-upload-pending')) return;
    draggedMedia = media;
    event.dataTransfer.effectAllowed = 'move';
    event.dataTransfer.setData('text/plain', 'gallery-placement');
  });

  rootBlockList.addEventListener('dragend', () => { draggedMedia = null; });

  rootBlockList.addEventListener('dragover', event => {
    const list = event.target instanceof Element ? event.target.closest('[data-gallery-list]') : null;
    if (!list) return;
    if (event.dataTransfer.files.length > 0 || draggedMedia) event.preventDefault();
  });

  rootBlockList.addEventListener('drop', event => {
    const list = event.target instanceof Element ? event.target.closest('[data-gallery-list]') : null;
    if (!list) return;
    if (event.dataTransfer.files.length > 0) {
      event.preventDefault();
      const root = list.closest('[data-draft-block]');
      const files = Array.from(event.dataTransfer.files);
      queueMediaUpload(root, files);
      return;
    }
    if (!draggedMedia) return;
    event.preventDefault();
    const target = event.target instanceof Element ? event.target.closest('[data-draft-media]') : null;
    if (target === draggedMedia) return;
    if (target) list.insertBefore(draggedMedia, target);
    else list.append(draggedMedia);
    draggedMedia = null;
    markDirty();
  });

  draftForm.addEventListener('submit', event => {
    event.preventDefault();
    queueOperation(() => saveDraftNow(true)).catch(() => {});
  });

  window.journeySaveDraft = () => {
    if (saveTimer) window.clearTimeout(saveTimer);
    saveTimer = null;
    return queueOperation(() => saveDraftNow());
  };

  updatePublishButton();
  loadExistingDraft();
}

const shareDialog = document.querySelector('#share-dialog');

if (shareDialog) {
  const closeShareButton = shareDialog.querySelector('.share-dialog-close');
  const previewFrame = document.querySelector('#share-preview');
  const fullPreviewLink = document.querySelector('#share-full-preview');
  const expiryLabel = document.querySelector('#share-expiry');
  const copyShareButton = document.querySelector('#share-copy');
  const revokeShareButton = document.querySelector('#share-revoke');
  const shareStatus = document.querySelector('#share-status');
  const shareHeading = document.querySelector('#share-dialog-heading');
  let activeShareButton = null;
  let currentPostId = null;
  let currentLink = null;
  let shareBusy = false;
  let panelGeneration = 0;
  let copiedMessageTimer = null;

  function clearShareStatus() {
    if (copiedMessageTimer) window.clearTimeout(copiedMessageTimer);
    copiedMessageTimer = null;
    shareStatus.classList.remove('is-error');
    shareStatus.textContent = '';
  }

  function showShareError(message) {
    clearShareStatus();
    shareStatus.classList.add('is-error');
    shareStatus.textContent = message;
  }

  function showCopiedMessage() {
    clearShareStatus();
    shareStatus.textContent = 'Link copied!';
    copiedMessageTimer = window.setTimeout(() => {
      shareStatus.textContent = '';
      copiedMessageTimer = null;
    }, 3000);
  }

  function updateShareControls() {
    copyShareButton.disabled = shareBusy;
    revokeShareButton.disabled = shareBusy;
    revokeShareButton.hidden = !currentLink;
  }

  function showExpiry(link) {
    const expiresAt = Number(link.expires_at_unix);
    if (!Number.isFinite(expiresAt)) {
      expiryLabel.textContent = 'Expiry unavailable.';
    } else {
      const date = new Date(expiresAt * 1000);
      expiryLabel.textContent = Number.isNaN(date.getTime())
        ? 'Expiry unavailable.'
        : `Link expires ${new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(date)}.`;
    }
    expiryLabel.hidden = false;
  }

  function resetSharePanel() {
    currentLink = null;
    currentPostId = null;
    expiryLabel.hidden = true;
    expiryLabel.textContent = '';
    revokeShareButton.hidden = true;
    clearShareStatus();
    fullPreviewLink.href = '#';
    previewFrame.removeAttribute('src');
    updateShareControls();
  }

  async function copyShareLink() {
    if (shareBusy || !currentPostId) return;
    shareBusy = true;
    const generation = panelGeneration;
    clearShareStatus();
    updateShareControls();

    try {
      if (!currentLink) {
        const response = await fetch(`/api/posts/${encodeURIComponent(currentPostId)}/share-links`, {
          method: 'POST',
          headers: { Accept: 'application/json' },
        });
        if (generation !== panelGeneration) return;
        if (response.status === 401) {
          redirectToLogin();
          return;
        }
        if (!response.ok) throw new Error('create');
        const link = await response.json();
        if (generation !== panelGeneration) return;
        if (typeof link.id !== 'string' || typeof link.url !== 'string' || !link.url) {
          throw new Error('create');
        }
        currentLink = link;
        showExpiry(link);
        updateShareControls();
      }

      try {
        await navigator.clipboard.writeText(currentLink.url);
      } catch (_) {
        if (generation === panelGeneration) {
          showShareError('Could not copy the link. Check clipboard permissions and try again.');
        }
        return;
      }
      if (generation === panelGeneration) showCopiedMessage();
    } catch (_) {
      if (generation === panelGeneration) showShareError('Could not create the share link. Try again.');
    } finally {
      shareBusy = false;
      updateShareControls();
    }
  }

  async function revokeShareLink() {
    if (shareBusy || !currentLink) return;
    shareBusy = true;
    const generation = panelGeneration;
    clearShareStatus();
    updateShareControls();

    try {
      const response = await fetch(`/api/share-links/${encodeURIComponent(currentLink.id)}`, {
        method: 'DELETE',
      });
      if (generation !== panelGeneration) return;
      if (response.status === 401) {
        redirectToLogin();
        return;
      }
      if (!response.ok) throw new Error('revoke');
      currentLink = null;
      expiryLabel.hidden = true;
      expiryLabel.textContent = '';
      revokeShareButton.hidden = true;
      shareStatus.textContent = 'Link revoked.';
    } catch (_) {
      if (generation === panelGeneration) showShareError('Could not revoke the link. Try again.');
    } finally {
      shareBusy = false;
      updateShareControls();
    }
  }

  document.addEventListener('click', event => {
    if (!(event.target instanceof Element)) return;
    const shareButton = event.target.closest('[data-share-post]');
    if (!shareButton) return;

    panelGeneration += 1;
    resetSharePanel();
    activeShareButton = shareButton;
    currentPostId = shareButton.dataset.sharePost;
    const post = shareButton.closest('.post');
    const title = post?.querySelector('h1')?.textContent?.trim();
    shareHeading.textContent = title ? `Share “${title}”` : 'Share post';
    const previewUrl = `/posts/${encodeURIComponent(currentPostId)}/share-preview`;
    fullPreviewLink.href = previewUrl;
    previewFrame.src = previewUrl;
    updateShareControls();
    shareDialog.showModal();
    closeShareButton.focus({ preventScroll: true });
  });

  copyShareButton.addEventListener('click', copyShareLink);
  revokeShareButton.addEventListener('click', revokeShareLink);
  closeShareButton.addEventListener('click', () => shareDialog.close());
  shareDialog.addEventListener('click', event => {
    if (event.target === shareDialog) shareDialog.close();
  });
  shareDialog.addEventListener('close', () => {
    panelGeneration += 1;
    resetSharePanel();
    if (activeShareButton?.isConnected) activeShareButton.focus({ preventScroll: true });
    activeShareButton = null;
  });
}

const feed = document.querySelector('#feed');

if (feed) {
  const status = document.querySelector('#feed-status');
  const loadButton = document.querySelector('#load-more');
  const sentinel = document.querySelector('#feed-sentinel');
  let nextCursor = feed.dataset.nextCursor || null;
  let loadingFeed = false;

  async function loadNextPost() {
    if (loadingFeed || !nextCursor) return;
    loadingFeed = true;
    loadButton.disabled = true;
    status.textContent = 'Loading the next post…';

    try {
      const feedQuery = new URLSearchParams({ limit: '1', after: nextCursor });
      if (feed.hasAttribute('data-tag')) feedQuery.set('tag', feed.dataset.tag);
      if (feed.hasAttribute('data-month')) feedQuery.set('month', feed.dataset.month);
      const feedResponse = await fetch(`/api/posts?${feedQuery}`);
      if (feedResponse.status === 401) {
        redirectToLogin();
        return;
      }
      if (!feedResponse.ok) throw new Error(`Feed request failed: ${feedResponse.status}`);
      const page = await feedResponse.json();
      if (!Array.isArray(page.posts)) throw new Error('Invalid feed response');

      if (page.posts.length === 0) {
        nextCursor = null;
      } else {
        const post = page.posts[0];
        const postResponse = await fetch(`/posts/${encodeURIComponent(post.id)}/fragment`);
        if (postResponse.status === 401) {
          redirectToLogin();
          return;
        }
        if (!postResponse.ok) throw new Error(`Post request failed: ${postResponse.status}`);
        const markup = await postResponse.text();
        const template = document.createElement('template');
        template.innerHTML = markup.trim();
        const article = template.content.firstElementChild;
        if (!article || !article.matches('article.post')) throw new Error('Invalid post fragment');
        feed.append(article);
        formatLocalTimes(article);
        initializeVideoPreviews(article);
        initializeGalleryVideoPreviews(article);
        nextCursor = typeof page.next_cursor === 'string' ? page.next_cursor : null;
      }

      feed.dataset.nextCursor = nextCursor || '';
      status.textContent = nextCursor ? '' : 'You have reached the end of the feed.';
    } catch (_) {
      status.textContent = 'Could not load the next post. Use Load more to retry.';
    } finally {
      loadingFeed = false;
      loadButton.disabled = !nextCursor;
    }
  }

  loadButton.addEventListener('click', loadNextPost);

  if ('IntersectionObserver' in window) {
    const observer = new IntersectionObserver(entries => {
      if (entries.some(entry => entry.isIntersecting)) loadNextPost();
    }, { rootMargin: '800px 0px' });
    observer.observe(sentinel);
  } else {
    window.addEventListener('scroll', () => {
      if (loadingFeed || !nextCursor) return;
      const rect = sentinel.getBoundingClientRect();
      if (rect.top <= window.innerHeight + 800) loadNextPost();
    }, { passive: true });
  }

  loadButton.disabled = !nextCursor;
}

const slideshow = document.querySelector('#slideshow');

if (slideshow) {
  const mediaRegion = document.querySelector('#slideshow-media');
  const labelRegion = document.querySelector('#slideshow-label');
  const captionRegion = document.querySelector('#slideshow-caption');
  const closeButton = slideshow.querySelector('.slideshow-close');
  const previousButton = slideshow.querySelector('.slideshow-previous');
  const nextButton = slideshow.querySelector('.slideshow-next');
  let slideItems = [];
  let slideIndex = 0;
  let openingItem = null;
  let touchStart = null;

  function renderSlide() {
    const item = slideItems[slideIndex];
    if (!item) return;
    const focusedElement = slideshow.contains(document.activeElement) ? document.activeElement : null;
    mediaRegion.replaceChildren();

    const mediaType = item.dataset.mediaType || '';
    const mediaUrl = item.dataset.mediaSrc || '';
    if (mediaType.startsWith('image/')) {
      const image = document.createElement('img');
      image.src = mediaUrl;
      image.alt = item.dataset.alt || '';
      mediaRegion.append(image);
    } else if (mediaType.startsWith('video/')) {
      const video = document.createElement('video');
      video.controls = true;
      video.preload = 'metadata';
      video.dataset.videoPreview = '';
      video.setAttribute('aria-label', item.dataset.label || `Video ${slideIndex + 1}`);
      initializeVideoPreview(video);
      const source = document.createElement('source');
      source.src = mediaUrl;
      source.type = mediaType;
      video.append(source);
      mediaRegion.append(video);
    }

    const itemLabel = item.dataset.label || '';
    labelRegion.textContent = itemLabel ? `${itemLabel} · ${slideIndex + 1} of ${slideItems.length}` : `${slideIndex + 1} of ${slideItems.length}`;
    captionRegion.textContent = item.dataset.caption || '';
    captionRegion.hidden = !captionRegion.textContent;
    previousButton.disabled = slideItems.length < 2;
    nextButton.disabled = slideItems.length < 2;
    slideshow.setAttribute('aria-label', `Photo slideshow, item ${slideIndex + 1} of ${slideItems.length}`);

    if (focusedElement && !focusedElement.isConnected) {
      nextButton.focus({ preventScroll: true });
    }
  }

  function moveSlide(direction) {
    if (slideItems.length < 2) return;
    slideIndex = (slideIndex + direction + slideItems.length) % slideItems.length;
    renderSlide();
  }

  function openSlideshow(item) {
    const galleryId = item.dataset.gallery;
    slideItems = Array.from(document.querySelectorAll('.gallery-item[data-gallery]'))
      .filter(candidate => candidate.dataset.gallery === galleryId);
    slideIndex = slideItems.indexOf(item);
    if (slideIndex < 0) return;
    openingItem = item;
    renderSlide();
    slideshow.showModal();
    closeButton.focus({ preventScroll: true });
  }

  document.addEventListener('click', event => {
    if (!(event.target instanceof Element)) return;
    const item = event.target.closest('.gallery-item[data-gallery]');
    if (item) openSlideshow(item);
  });

  closeButton.addEventListener('click', () => slideshow.close());
  previousButton.addEventListener('click', () => moveSlide(-1));
  nextButton.addEventListener('click', () => moveSlide(1));

  slideshow.addEventListener('click', event => {
    if (event.target === slideshow) slideshow.close();
  });

  slideshow.addEventListener('close', () => {
    mediaRegion.replaceChildren();
    if (openingItem && openingItem.isConnected) openingItem.focus({ preventScroll: true });
    openingItem = null;
    slideItems = [];
    slideIndex = 0;
  });

  document.addEventListener('keydown', event => {
    if (!slideshow.open) return;
    if (event.key === 'Escape') {
      event.preventDefault();
      slideshow.close();
    } else if (event.key === 'ArrowLeft') {
      event.preventDefault();
      moveSlide(-1);
    } else if (event.key === 'ArrowRight') {
      event.preventDefault();
      moveSlide(1);
    }
  });

  slideshow.addEventListener('touchstart', event => {
    if (event.changedTouches.length === 1) {
      touchStart = { x: event.changedTouches[0].clientX, y: event.changedTouches[0].clientY };
    }
  }, { passive: true });

  slideshow.addEventListener('touchend', event => {
    if (!touchStart || event.changedTouches.length !== 1) return;
    const deltaX = event.changedTouches[0].clientX - touchStart.x;
    const deltaY = event.changedTouches[0].clientY - touchStart.y;
    touchStart = null;
    if (Math.abs(deltaX) > 55 && Math.abs(deltaX) > Math.abs(deltaY)) {
      moveSlide(deltaX < 0 ? 1 : -1);
    }
  }, { passive: true });
}
