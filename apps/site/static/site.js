const sidebarLayout = document.querySelector('#site-layout');

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
      if (!feedResponse.ok) throw new Error(`Feed request failed: ${feedResponse.status}`);
      const page = await feedResponse.json();
      if (!Array.isArray(page.posts)) throw new Error('Invalid feed response');

      if (page.posts.length === 0) {
        nextCursor = null;
      } else {
        const post = page.posts[0];
        const postResponse = await fetch(`/posts/${encodeURIComponent(post.id)}/fragment`);
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
