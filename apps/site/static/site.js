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
        errorMessage.textContent = returnedError || 'Could not publish this post. Try again.';
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
  let draftBusy = false;

  function clearDraftError() {
    errorMessage.hidden = true;
    errorMessage.textContent = '';
  }

  function showDraftError(message) {
    errorMessage.textContent = message;
    errorMessage.hidden = false;
  }

  function makeActionButton(label, action, className = '') {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = `draft-small-button ${className}`.trim();
    button.dataset.blockAction = action;
    button.textContent = label;
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

  function makeDraftBlock(isChild = false) {
    const block = document.createElement('fieldset');
    block.className = isChild ? 'draft-block draft-child-block' : 'draft-block';
    block.dataset.draftBlock = '';

    const legend = document.createElement('legend');
    legend.textContent = isChild ? 'Child block' : 'Text block';
    block.append(legend);

    const actions = document.createElement('div');
    actions.className = 'draft-block-actions';
    actions.append(
      makeActionButton('Move up', 'move-up'),
      makeActionButton('Move down', 'move-down'),
      makeActionButton('Remove', 'remove', 'draft-remove-button'),
    );
    block.append(actions);

    const fields = document.createElement('div');
    fields.className = 'draft-block-fields';
    fields.append(makeBlockField('Header (optional)', 'header', false));
    fields.append(makeBlockField('Body (optional)', 'body', true));
    block.append(fields);

    if (!isChild) {
      const childArea = document.createElement('div');
      childArea.className = 'draft-child-area';
      const childHeading = document.createElement('h3');
      childHeading.textContent = 'Child blocks';
      const childList = document.createElement('div');
      childList.className = 'draft-child-list';
      childList.dataset.blockList = 'child';
      const addChildButton = makeActionButton('Add child block', 'add-child');
      childArea.append(childHeading, childList, addChildButton);
      block.append(childArea);
    }

    return block;
  }

  function updateDraftBlockControls() {
    document.querySelectorAll('[data-block-list]').forEach(list => {
      const blocks = Array.from(list.children).filter(child => child.matches('[data-draft-block]'));
      blocks.forEach((block, index) => {
        const legend = block.querySelector(':scope > legend');
        const actions = block.querySelector(':scope > .draft-block-actions');
        const isChild = list.dataset.blockList === 'child';
        const label = `${isChild ? 'Child block' : 'Text block'} ${index + 1}`;
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
    const field = block.querySelector(`:scope > .draft-block-fields [data-block-field="${name}"]`);
    return field.value.trim() ? field.value : null;
  }

  function serializeBlock(block) {
    const childList = block.querySelector(':scope > .draft-child-area > [data-block-list="child"]');
    const children = childList
      ? Array.from(childList.children).filter(child => child.matches('[data-draft-block]')).map(serializeBlock)
      : [];
    return {
      header: blockFieldValue(block, 'header'),
      body: blockFieldValue(block, 'body'),
      blocks: children,
    };
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
      return;
    }

    if (action === 'add-child') {
      const parent = button.closest('[data-draft-block]');
      const childList = parent?.querySelector(':scope > .draft-child-area > [data-block-list="child"]');
      if (!childList) return;
      const block = makeDraftBlock(true);
      childList.append(block);
      updateDraftBlockControls();
      block.querySelector('[data-block-field="header"]').focus();
      return;
    }

    const block = button.closest('[data-draft-block]');
    const list = block?.parentElement;
    if (!block || !list) return;
    const siblings = Array.from(list.children).filter(child => child.matches('[data-draft-block]'));
    const index = siblings.indexOf(block);
    if (action === 'remove') {
      const nextFocus = siblings[index + 1] || siblings[index - 1];
      const parentAddChild = list.closest('[data-draft-block]')
        ?.querySelector(':scope > .draft-child-area > [data-block-action="add-child"]');
      block.remove();
      updateDraftBlockControls();
      (nextFocus?.querySelector('[data-block-field="header"]') || parentAddChild || document.querySelector('[data-block-action="add-root"]')).focus();
    } else if (action === 'move-up' && index > 0) {
      list.insertBefore(block, siblings[index - 1]);
      updateDraftBlockControls();
      button.focus();
    } else if (action === 'move-down' && index < siblings.length - 1) {
      list.insertBefore(siblings[index + 1], block);
      updateDraftBlockControls();
      button.focus();
    }
  });

  titleInput.addEventListener('input', () => {
    titleInput.removeAttribute('aria-invalid');
    clearDraftError();
  });

  draftForm.addEventListener('submit', async event => {
    event.preventDefault();
    if (draftBusy) return;
    clearDraftError();
    if (!titleInput.value.trim()) {
      titleInput.setAttribute('aria-invalid', 'true');
      showDraftError('Enter a title before creating this draft.');
      titleInput.focus();
      return;
    }

    const payload = {
      title: titleInput.value,
      summary: summaryInput.value,
      tags: tagsInput.value.split(',').map(tag => tag.trim()).filter(Boolean),
      blocks: Array.from(rootBlockList.children)
        .filter(block => block.matches('[data-draft-block]'))
        .map(serializeBlock),
    };

    draftBusy = true;
    submitButton.disabled = true;
    submitButton.textContent = 'Creating…';
    draftForm.setAttribute('aria-busy', 'true');

    let failureMessage = 'Could not create the draft. Your entries are still here; please try again.';

    try {
      const response = await fetch('/api/posts', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload),
      });
      if (!response.ok) {
        if (response.status === 401) failureMessage = 'Your session is no longer active. Your entries are still here; sign in again before retrying.';
        if (response.status === 403) failureMessage = 'This account is not allowed to create drafts. Your entries are still here.';
        showDraftError(failureMessage);
        return;
      }
      const created = await response.json();
      if (!created || !Number.isInteger(created.id) || created.id < 1) {
        failureMessage = 'The server could not confirm the new draft. Your entries are still here; please try again.';
        showDraftError(failureMessage);
        return;
      }
      window.location.assign(`/posts/${encodeURIComponent(created.id)}?created=1`);
    } catch (_) {
      showDraftError(failureMessage);
    } finally {
      draftBusy = false;
      submitButton.disabled = false;
      submitButton.textContent = 'Create draft';
      draftForm.removeAttribute('aria-busy');
    }
  });
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
      video.setAttribute('aria-label', item.dataset.label || `Video ${slideIndex + 1}`);
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
