// Derive the active page's ancestor chain from mdbook's rendered sidebar
// and inject a breadcrumb at the top of the content area.

(function () {
  const HOME_LABEL_FALLBACK = 'Home';

  function init() {
    // mdbook's sidebar paints asynchronously via toc.js; observe and retry
    // until the active link appears.
    let observer = null;
    let timer = 0;
    const tryRender = () => {
      const main = document.querySelector('main');
      if (!main) return false;
      if (main.querySelector('.mdp-breadcrumb')) return true;
      const chain = buildChain();
      if (!chain) return false;
      // A 1-crumb breadcrumb (just the active page) duplicates the H1.
      if (chain.length < 2) return true;

      const nav = document.createElement('nav');
      nav.className = 'mdp-breadcrumb';
      nav.setAttribute('aria-label', 'Breadcrumb');
      const ol = document.createElement('ol');
      ol.className = 'mdp-breadcrumb-list';
      chain.forEach((c, i) => {
        const li = document.createElement('li');
        li.className = 'mdp-breadcrumb-item';
        const isLast = i === chain.length - 1;
        if (isLast) {
          const span = document.createElement('span');
          span.className = 'mdp-breadcrumb-current';
          span.setAttribute('aria-current', 'page');
          span.textContent = c.text;
          if (c.text.length > 32) span.title = c.text;
          li.appendChild(span);
        } else if (c.href) {
          const a = document.createElement('a');
          a.href = c.href;
          a.textContent = c.text;
          if (c.text.length > 32) a.title = c.text;
          li.appendChild(a);
        } else {
          const span = document.createElement('span');
          span.textContent = c.text;
          if (c.text.length > 32) span.title = c.text;
          li.appendChild(span);
        }
        if (!isLast) {
          const sep = document.createElement('span');
          sep.className = 'mdp-breadcrumb-sep';
          sep.setAttribute('aria-hidden', 'true');
          sep.textContent = '›';
          li.appendChild(sep);
        }
        ol.appendChild(li);
      });
      nav.appendChild(ol);
      main.prepend(nav);
      return true;
    };

    const teardown = () => {
      if (observer) {
        observer.disconnect();
        observer = null;
      }
      if (timer) {
        clearTimeout(timer);
        timer = 0;
      }
    };

    // Attach the observer BEFORE the first synchronous tryRender so a paint
    // that lands between `tryRender returned false` and `observe()` can't
    // be missed. If the sync render wins, teardown immediately disconnects.
    observer = new MutationObserver(() => {
      if (tryRender()) teardown();
    });
    observer.observe(document.body, { childList: true, subtree: true });

    if (tryRender()) {
      teardown();
      return;
    }

    // Stop observing after 5s even if the sidebar never paints, so the
    // closure doesn't outlive the page.
    timer = setTimeout(teardown, 5000);
  }

  function buildChain() {
    const sidebar =
      document.getElementById('mdbook-sidebar') ||
      document.querySelector('.sidebar');
    if (!sidebar) return null;

    const active = sidebar.querySelector('a.active');
    if (!active) return null;

    const chain = [];
    let li = active.closest('li');
    while (li) {
      // Exclude `.chapter-fold-toggle`; mdbook renders it as
      // `<a class="chapter-fold-toggle"><div>❱</div></a>` inside
      // `.chapter-link-wrapper`, so a naive `:scope > .chapter-link-wrapper > a`
      // matches the toggle on draft chapters and yields a `❱` crumb.
      const link =
        li.querySelector(':scope > .chapter-link-wrapper > a:not(.chapter-fold-toggle)') ||
        li.querySelector(':scope > a:not(.chapter-fold-toggle)');
      // Draft chapters render as a plain `<span>` sibling of the toggle.
      const fallbackSpan = li.querySelector(
        ':scope > .chapter-link-wrapper > span:not(.chapter-fold-toggle)'
      );
      const text = (link?.textContent || fallbackSpan?.textContent || '')
        .replace(/^\s*\d+(\.\d+)*\.\s*/, '')
        .trim();
      if (text) {
        chain.unshift({
          text,
          href: link?.getAttribute('href') || null,
        });
      }
      li = li.parentElement?.closest('li') || null;
    }

    if (chain.length === 0) return null;

    const activeHref = active.getAttribute('href') || '';
    if (!isRootIndex(activeHref)) {
      chain.unshift({ text: bookTitle(), href: rootIndexHref() });
    }
    return chain;
  }

  function rootIndexHref() {
    if (typeof window !== 'undefined' && typeof window.path_to_root === 'string') {
      return window.path_to_root + 'index.html';
    }
    const metaPtr = document.querySelector('meta[name="path-to-root"]')?.content;
    if (metaPtr) return metaPtr + 'index.html';
    const baseHref = document.querySelector('base')?.getAttribute('href');
    if (baseHref) return new URL('index.html', baseHref).href;
    return './index.html';
  }

  function bookTitle() {
    const t = document.title || '';
    const m = t.match(/^.+\s[-–—]\s(.+)$/);
    if (m && m[1]) return m[1].trim();
    return HOME_LABEL_FALLBACK;
  }

  function isRootIndex(href) {
    if (!href) return true;
    const clean = href.split('?')[0].split('#')[0];
    return clean === 'index.html' || clean === './index.html';
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
