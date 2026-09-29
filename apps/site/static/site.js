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
