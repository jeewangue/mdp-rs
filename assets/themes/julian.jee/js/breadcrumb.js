// mdp breadcrumb — derives the active page's ancestor chain from the rendered
// mdbook sidebar and injects a `Home › Section › Page` nav at the top of the
// content area. Pure DOM, no framework.
//
// Why not a Handlebars theme override? mdbook's theme/index.hbs is upstream
// API surface that changes between minor versions. This script reads the
// stable `<ol class="chapter">` + `<li class="chapter-item">` shape produced
// by mdbook's TOC renderer, which has been backwards-compatible since 0.4.

(function () {
  const HOME_LABEL_FALLBACK = 'Home';

  function init() {
    // mdbook's sidebar renders into <mdbook-sidebar-scrollbox> via toc.js. It
    // may not be ready immediately on first paint; observe and try once it is.
    let observer = null;
    const tryRender = () => {
      const main = document.querySelector('main');
      if (!main) return false;
      if (main.querySelector('.mdp-breadcrumb')) return true; // already rendered
      const chain = buildChain();
      if (!chain) return false;
      // Suppress trivial breadcrumbs — a single-crumb nav (the active page
      // alone, no ancestors) is visual noise that just duplicates the H1.
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
          // Draft chapter — no link, render as plain text.
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

    if (tryRender()) return;
    let timer = 0;
    observer = new MutationObserver(() => {
      if (tryRender()) {
        observer.disconnect();
        observer = null;
        if (timer) {
          clearTimeout(timer);
          timer = 0;
        }
      }
    });
    observer.observe(document.body, { childList: true, subtree: true });
    // Safety: stop observing after 5s even if the sidebar never paints. The
    // timer is cleared when the observer wins naturally so we don't leak the
    // closure across the page lifetime.
    timer = setTimeout(() => {
      if (observer) {
        observer.disconnect();
        observer = null;
      }
      timer = 0;
    }, 5000);
  }

  function buildChain() {
    const sidebar =
      document.getElementById('mdbook-sidebar') ||
      document.querySelector('.sidebar');
    if (!sidebar) return null;

    // Find the active <a>. mdbook sets `class="active"` on the chapter link
    // for the current page after toc.js populates the sidebar. If the active
    // link isn't found yet, the MutationObserver retries until it is.
    const active = sidebar.querySelector('a.active');
    if (!active) return null;

    const chain = [];
    let li = active.closest('li');
    while (li) {
      // Exclude `.chapter-fold-toggle` — mdbook renders it as `<a><div>❱</div></a>`
      // inside `.chapter-link-wrapper`, so a naive `:scope > .chapter-link-wrapper > a`
      // would match the toggle on draft chapters (those rendered as <span>, like
      // a directory with no index.md/README.md) and produce a `❱` crumb.
      const link =
        li.querySelector(':scope > .chapter-link-wrapper > a:not(.chapter-fold-toggle)') ||
        li.querySelector(':scope > a:not(.chapter-fold-toggle)');
      // Draft chapter (no link) — mdbook renders the title as a plain <span>
      // sibling of the toggle. Pull text from there.
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

    // Always link the topmost crumb to the root index unless we're already on
    // it. Compute the root URL via mdbook's `path-to-root` (set on every
    // page's <html>) — this is the ONLY stable contract for relative root
    // resolution across nested pages.
    const activeHref = active.getAttribute('href') || '';
    if (!isRootIndex(activeHref)) {
      chain.unshift({ text: bookTitle(), href: rootIndexHref() });
    }
    return chain;
  }

  // mdbook's book.js sets `path_to_root` on the global window; the rendered
  // HTML also exposes it as a `<meta name="path-to-root">` in some templates.
  // Fall back gracefully across the variants we've seen.
  function rootIndexHref() {
    if (typeof window !== 'undefined' && typeof window.path_to_root === 'string') {
      return window.path_to_root + 'index.html';
    }
    const metaPtr = document.querySelector('meta[name="path-to-root"]')?.content;
    if (metaPtr) return metaPtr + 'index.html';
    // Last-resort: count slashes in the URL pathname relative to the book's
    // base href. Won't be precise for non-trivial mounts, but better than a
    // broken link.
    const baseHref = document.querySelector('base')?.getAttribute('href');
    if (baseHref) return new URL('index.html', baseHref).href;
    return './index.html';
  }

  function bookTitle() {
    // Prefer the book's <title> stripped of the page suffix, fall back to a
    // generic label. mdbook renders `<title>Page - Book</title>`; we want
    // "Book". If we can't tell, use the literal "Home".
    const t = document.title || '';
    const m = t.match(/^.+\s[-–—]\s(.+)$/);
    if (m && m[1]) return m[1].trim();
    return HOME_LABEL_FALLBACK;
  }

  function isRootIndex(href) {
    if (!href) return true;
    // Strip query string + hash for comparison.
    const clean = href.split('?')[0].split('#')[0];
    return clean === 'index.html' || clean === './index.html';
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
