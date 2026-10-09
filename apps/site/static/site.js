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

function initializeGalleryMediaImages(root = document) {
  const images = [];
  const selector = 'img[data-video-thumbnail], img.gallery-panel-image';
  if (root.matches?.(selector)) images.push(root);
  images.push(...root.querySelectorAll?.(selector) || []);
  images.forEach(image => {
    if (image.hasAttribute('data-src')) return;
    if (image.complete && image.naturalWidth > 0 && image.hasAttribute('data-video-thumbnail')) {
      image.closest('[data-video-preview-frame]')?.setAttribute('data-ready', '');
    }
    updateGalleryMediaAspectRatio(image);
  });
}

function updateGalleryMediaAspectRatio(image) {
  if (image.naturalWidth === 0 || image.naturalHeight === 0) return;
  image.closest('.gallery-panel-media')?.style.setProperty(
    '--gallery-media-ratio',
    `${image.naturalWidth} / ${image.naturalHeight}`,
  );
}

function initializeVideoControls(video, container = video?.parentElement) {
  if (!(video instanceof HTMLVideoElement) || video.dataset.videoControlsReady !== undefined || !container) return;

  video.playsInline = true;
  const button = document.createElement('button');
  button.className = 'video-play-toggle';
  button.type = 'button';
  const icon = document.createElement('span');
  icon.className = 'video-play-toggle-icon';
  icon.setAttribute('aria-hidden', 'true');
  button.append(icon);

  const label = video.getAttribute('aria-label') || 'video';
  const update = () => {
    const playing = !video.paused && !video.ended;
    icon.textContent = playing ? 'Ⅱ' : '▶';
    button.setAttribute('aria-label', `${playing ? 'Pause' : 'Play'} ${label}`);
    button.dataset.playing = String(playing);
  };

  button.addEventListener('click', () => {
    if (video.paused || video.ended) {
      try {
        video.play().catch(update);
      } catch (_) {
        update();
      }
    } else {
      video.pause();
      update();
    }
  });
  video.addEventListener('play', update);
  video.addEventListener('pause', update);
  video.addEventListener('ended', update);
  container.classList.add('video-player');
  container.append(button);
  video.dataset.videoControlsReady = '';
  update();
}

function createVideoPlayerFrame(video, className = '') {
  const frame = document.createElement('div');
  frame.className = `video-player ${className}`.trim();
  frame.append(video);
  initializeVideoControls(video, frame);
  return frame;
}

document.querySelectorAll('video[data-video-controls]').forEach(video => initializeVideoControls(video));

document.addEventListener('play', event => {
  const playingVideo = event.target;
  if (!(playingVideo instanceof HTMLVideoElement)) return;
  document.querySelectorAll('video').forEach(video => {
    if (video !== playingVideo && !video.paused) video.pause();
  });
}, true);

document.addEventListener('load', event => {
  const image = event.target;
  if (!(image instanceof HTMLImageElement)) return;
  if (image.hasAttribute('data-video-thumbnail')) {
    image.hidden = false;
    image.closest('[data-video-preview-frame]')?.setAttribute('data-ready', '');
  }
  updateGalleryMediaAspectRatio(image);
}, true);

document.addEventListener('error', event => {
  const image = event.target;
  if (!(image instanceof HTMLImageElement) || !image.hasAttribute('data-video-thumbnail')) return;
  image.hidden = true;
  image.closest('[data-video-preview-frame]')?.removeAttribute('data-ready');
}, true);

initializeGalleryMediaImages();

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
const newPostEditorDialog = document.querySelector('#new-post-editor-dialog');

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
  let queuedOperationCount = 0;
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
    queuedOperationCount += 1;
    const result = operationQueue.then(operation);
    operationQueue = result.catch(() => {}).finally(() => { queuedOperationCount -= 1; });
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
    const mediaUrl = postId && media.id
      ? `/posts/${encodeURIComponent(postId)}/blocks/${encodeURIComponent(media.id)}/media`
      : '';
    const contentType = String(media.content_type || '');
    const source = contentType.startsWith('image/') ? (media.previewUrl || mediaUrl) : mediaUrl;
    if (source && contentType.startsWith('image/')) {
      const image = document.createElement('img');
      image.src = source;
      image.alt = media.alt || '';
      preview.append(image);
    } else if (contentType.startsWith('video/')) {
      if (source) {
        const video = document.createElement('video');
        video.preload = 'none';
        video.controls = true;
        video.poster = `${source}?thumbnail=1`;
        video.setAttribute('aria-label', media.header || media.body || 'video');
        video.src = source;
        preview.append(createVideoPlayerFrame(video, 'draft-video-player'));
      } else {
        const frame = document.createElement('div');
        frame.className = 'video-preview-frame draft-video-placeholder';
        frame.dataset.videoPreviewFrame = '';
        frame.setAttribute('aria-hidden', 'true');
        const placeholder = document.createElement('span');
        placeholder.className = 'video-placeholder';
        placeholder.textContent = 'Video';
        frame.append(placeholder);
        preview.append(frame);
      }
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
        if (video) {
          video.poster = `${mediaUrl}?thumbnail=1`;
          video.src = mediaUrl;
        } else if (String(child.content_type || '').startsWith('video/')) {
          const savedVideo = document.createElement('video');
          savedVideo.preload = 'none';
          savedVideo.controls = true;
          savedVideo.poster = `${mediaUrl}?thumbnail=1`;
          savedVideo.setAttribute('aria-label', child.header || child.body || 'video');
          savedVideo.src = mediaUrl;
          preview.replaceChildren(createVideoPlayerFrame(savedVideo, 'draft-video-player'));
        }
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
      if (wasNew && (!newPostEditorDialog || newPostEditorDialog.open)) {
        window.history.replaceState(window.history.state, '', `/posts/${encodeURIComponent(postId)}`);
      }
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

  const newPostLink = document.querySelector('.new-post-float');
  const mobileNewPostLayout = window.matchMedia(
    '(max-width: 600px), (orientation: landscape) and (max-height: 500px) and (pointer: coarse)',
  ).matches;
  if (newPostEditorDialog && newPostLink && !postId && mobileNewPostLayout) {
    const photoInput = document.querySelector('#new-post-overlay-photo-input');

    function resetNewDraftEditor() {
      if (saveTimer) window.clearTimeout(saveTimer);
      saveTimer = null;
      rootBlockList.querySelectorAll('[data-preview-url]').forEach(item => URL.revokeObjectURL(item.dataset.previewUrl));
      rootBlockList.replaceChildren();
      titleInput.value = '';
      summaryInput.value = '';
      tagsInput.value = '';
      postId = null;
      revision = null;
      editVersion = 0;
      dirty = false;
      draftForm.dataset.draftPostId = '';
      clearDraftError();
      updateDraftBlockControls();
      updatePublishButton();
      updateStatus('Not saved yet.');
    }

    function addChosenPhotos() {
      const files = Array.from(photoInput.files || []);
      photoInput.value = '';
      if (files.length === 0) return;
      const block = makeDraftBlock();
      rootBlockList.append(block);
      updateDraftBlockControls();
      block.querySelector('[data-block-field="header"]').focus({ preventScroll: true });
      queueMediaUpload(block, files);
    }

    newPostLink.addEventListener('click', event => {
      event.preventDefault();
      const hasPendingUploads = Boolean(rootBlockList.querySelector('[data-upload-pending]'));
      const hasUnfinishedWork = postId || dirty || saveTimer || queuedOperationCount > 0
        || draftForm.hasAttribute('aria-busy') || hasPendingUploads;
      if (postId && !dirty && !saveTimer && queuedOperationCount === 0
        && !draftForm.hasAttribute('aria-busy') && !hasPendingUploads) {
        resetNewDraftEditor();
      } else if (hasUnfinishedWork) {
        if (saveTimer) window.clearTimeout(saveTimer);
        saveTimer = null;
        queueOperation(() => saveDraftNow(true)).then(() => {
          window.location.assign(newPostLink.href);
        }).catch(() => {
          newPostEditorDialog.showModal();
          window.history.pushState(
            { journeyNewPostOverlay: true },
            '',
            postId ? `/posts/${encodeURIComponent(postId)}` : '/posts/new',
          );
        });
        return;
      }
      newPostEditorDialog.showModal();
      window.history.pushState({ journeyNewPostOverlay: true }, '', '/posts/new');
      photoInput.click();
    });
    photoInput.addEventListener('change', addChosenPhotos);
    photoInput.addEventListener('cancel', () => titleInput.focus({ preventScroll: true }));
    newPostEditorDialog.querySelector('.new-post-editor-close').addEventListener('click', () => newPostEditorDialog.close());
    newPostEditorDialog.addEventListener('click', event => {
      if (event.target === newPostEditorDialog) newPostEditorDialog.close();
    });
    newPostEditorDialog.addEventListener('close', () => {
      if (window.history.state?.journeyNewPostOverlay) window.history.back();
      if (newPostLink.isConnected) newPostLink.focus({ preventScroll: true });
    });
    window.addEventListener('popstate', () => {
      if (window.history.state?.journeyNewPostOverlay) {
        if (!newPostEditorDialog.open) newPostEditorDialog.showModal();
      } else if (newPostEditorDialog.open) {
        newPostEditorDialog.close();
      }
    });
  }

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
    let loaded = false;

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
        initializeGalleryMediaImages(article);
        nextCursor = typeof page.next_cursor === 'string' ? page.next_cursor : null;
      }

      feed.dataset.nextCursor = nextCursor || '';
      status.textContent = nextCursor ? '' : 'You have reached the end of the feed.';
      loaded = true;
    } catch (_) {
      status.textContent = 'Could not load the next post. Use Load more to retry.';
    } finally {
      loadingFeed = false;
      loadButton.disabled = !nextCursor;
      if (loaded && nextCursor) {
        const bounds = sentinel.getBoundingClientRect();
        if (bounds.bottom >= -800 && bounds.top <= window.innerHeight + 800) {
          window.requestAnimationFrame(loadNextPost);
        }
      }
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

const galleryPanel = document.querySelector('#gallery-panel');

if (galleryPanel) {
  const galleryRegion = document.querySelector('#gallery-panel-items');
  const closeButton = galleryPanel.querySelector('.gallery-panel-close');
  let openingItem = null;
  let panelMediaObserver = null;
  let panelVisibilityCheckScheduled = false;

  function loadPanelEntry(entry) {
    if (!galleryPanel.open || !entry.isConnected) return;
    if (entry.dataset.panelLoaded) return;
    entry.dataset.panelLoaded = 'true';
    entry.querySelectorAll('img[data-src]').forEach(image => {
      image.src = image.dataset.src;
      image.removeAttribute('data-src');
    });
    initializeGalleryMediaImages(entry);
  }

  function checkPanelMediaVisibility() {
    panelVisibilityCheckScheduled = false;
    if (!galleryPanel.open) return;
    const regionBounds = galleryRegion.getBoundingClientRect();
    const margin = 800;
    galleryRegion.querySelectorAll('.gallery-panel-entry:not([data-panel-loaded])').forEach(entry => {
      const bounds = entry.getBoundingClientRect();
      if (bounds.bottom >= regionBounds.top - margin && bounds.top <= regionBounds.bottom + margin) {
        loadPanelEntry(entry);
      }
    });
  }

  function schedulePanelMediaVisibilityCheck() {
    if (panelVisibilityCheckScheduled) return;
    panelVisibilityCheckScheduled = true;
    window.requestAnimationFrame(checkPanelMediaVisibility);
  }

  function observePanelMedia() {
    const entries = galleryRegion.querySelectorAll('.gallery-panel-entry:not([data-panel-loaded])');
    if ('IntersectionObserver' in window) {
      const observer = new IntersectionObserver(observations => {
        observations.forEach(observation => {
          if (!observation.isIntersecting || !galleryPanel.open || !observation.target.isConnected) return;
          observer.unobserve(observation.target);
          loadPanelEntry(observation.target);
        });
      }, { root: galleryRegion, rootMargin: '800px 0px' });
      panelMediaObserver = observer;
      entries.forEach(entry => observer.observe(entry));
      return;
    }

    galleryRegion.addEventListener('scroll', schedulePanelMediaVisibilityCheck, { passive: true });
    window.addEventListener('resize', schedulePanelMediaVisibilityCheck, { passive: true });
    schedulePanelMediaVisibilityCheck();
  }

  function createPanelEntry(item, index) {
    const mediaType = item.dataset.mediaType || '';
    const mediaUrl = item.dataset.mediaSrc || '';
    const itemLabel = item.dataset.label || '';
    const article = document.createElement('article');
    article.className = 'gallery-panel-entry';
    article.id = `gallery-panel-item-${index + 1}`;

    const media = document.createElement('div');
    media.className = 'gallery-panel-media';
    media.dataset.mediaSrc = mediaUrl;

    if (mediaType.startsWith('image/')) {
      media.classList.add('gallery-panel-image-media');
      const image = document.createElement('img');
      image.className = 'gallery-panel-image';
      image.alt = item.dataset.alt || '';
      image.dataset.src = mediaUrl;
      media.append(image);
    } else if (mediaType.startsWith('video/')) {
      media.classList.add('gallery-panel-video-media');
      const frame = document.createElement('div');
      frame.className = 'video-preview-frame gallery-panel-video-frame';
      frame.dataset.videoPreviewFrame = '';
      const thumbnail = document.createElement('img');
      thumbnail.className = 'video-thumbnail gallery-panel-video-thumbnail';
      thumbnail.alt = '';
      thumbnail.dataset.src = item.dataset.thumbnailSrc || '';
      thumbnail.dataset.videoThumbnail = '';
      thumbnail.loading = 'lazy';

      const placeholder = document.createElement('div');
      placeholder.className = 'video-placeholder';
      const videoLabel = document.createElement('span');
      videoLabel.textContent = 'Video';
      const playButton = document.createElement('button');
      playButton.className = 'gallery-panel-play';
      playButton.type = 'button';
      playButton.textContent = 'Play video';
      playButton.setAttribute('aria-label', itemLabel ? `Play ${itemLabel}` : `Play video ${index + 1}`);
      playButton.addEventListener('click', () => {
        if (!frame.isConnected) return;
        const player = document.createElement('video');
        player.className = 'gallery-panel-player';
        player.controls = true;
        player.playsInline = true;
        player.preload = 'none';
        player.poster = item.dataset.thumbnailSrc || '';
        player.setAttribute('aria-label', itemLabel || `Video ${index + 1}`);
        player.addEventListener('loadedmetadata', () => {
          if (!media.isConnected || player.videoWidth === 0 || player.videoHeight === 0) return;
          media.style.setProperty('--gallery-media-ratio', `${player.videoWidth} / ${player.videoHeight}`);
        });
        const playerFrame = createVideoPlayerFrame(player, 'gallery-panel-player-frame');
        frame.replaceChildren(playerFrame);
        player.src = mediaUrl;
        player.play().catch(() => {});
      });
      placeholder.append(videoLabel, playButton);
      frame.append(thumbnail, placeholder);
      media.append(frame);
    }

    const copy = document.createElement('div');
    copy.className = 'gallery-panel-copy';
    if (itemLabel) {
      const heading = document.createElement('h2');
      heading.textContent = itemLabel;
      copy.append(heading);
      article.setAttribute('aria-labelledby', `${article.id}-heading`);
      heading.id = `${article.id}-heading`;
    } else {
      article.setAttribute('aria-label', `${mediaType.startsWith('video/') ? 'Video' : 'Image'} ${index + 1}`);
    }
    const caption = item.dataset.caption || '';
    if (caption) {
      const description = document.createElement('p');
      description.textContent = caption;
      copy.append(description);
    }

    article.append(media);
    if (copy.childElementCount > 0) article.append(copy);
    return article;
  }

  function openGalleryPanel(item) {
    const galleryId = item.dataset.gallery;
    const items = Array.from(document.querySelectorAll('.gallery-item[data-gallery]'))
      .filter(candidate => candidate.dataset.gallery === galleryId);
    const selectedIndex = items.indexOf(item);
    if (selectedIndex < 0) return;

    openingItem = item;
    const fragment = document.createDocumentFragment();
    items.forEach((galleryItem, index) => fragment.append(createPanelEntry(galleryItem, index)));
    galleryRegion.replaceChildren(fragment);
    galleryPanel.showModal();

    const selectedEntry = galleryRegion.children[selectedIndex];
    const regionBounds = galleryRegion.getBoundingClientRect();
    const entryBounds = selectedEntry.getBoundingClientRect();
    const topPadding = Number.parseFloat(window.getComputedStyle(galleryRegion).paddingTop) || 0;
    galleryRegion.scrollTop += entryBounds.top - regionBounds.top - topPadding;
    loadPanelEntry(selectedEntry);
    observePanelMedia();
    closeButton.focus({ preventScroll: true });
  }

  document.addEventListener('click', event => {
    if (!(event.target instanceof Element)) return;
    const item = event.target.closest('.gallery-item[data-gallery]');
    if (item) openGalleryPanel(item);
  });

  galleryPanel.addEventListener('click', event => {
    if (event.target === galleryPanel) galleryPanel.close();
  });

  galleryPanel.addEventListener('close', () => {
    panelMediaObserver?.disconnect();
    panelMediaObserver = null;
    galleryRegion.removeEventListener('scroll', schedulePanelMediaVisibilityCheck);
    window.removeEventListener('resize', schedulePanelMediaVisibilityCheck);
    panelVisibilityCheckScheduled = false;

    galleryRegion.querySelectorAll('img').forEach(image => {
      image.removeAttribute('src');
      image.removeAttribute('srcset');
    });
    galleryRegion.querySelectorAll('video').forEach(video => {
      video.pause();
      video.removeAttribute('src');
      video.querySelectorAll('source').forEach(source => source.remove());
      video.load();
    });
    galleryRegion.replaceChildren();
    if (openingItem && openingItem.isConnected) openingItem.focus({ preventScroll: true });
    openingItem = null;
  });
}
