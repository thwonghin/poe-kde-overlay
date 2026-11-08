(function () {
  // Marker labels by app_i18n.json path; English first, then the active
  // language's translation once loaded (see loadLanguage).
  var MARKER_KEYS = {
    clearHotkey: ['settings', 'clear_hotkey'],
    itemCheckFirst: ['item', 'open_on_wiki'],
    autoSearch: ['price_check', 'auto_search']
  };
  var markers = {
    clearHotkey: ['You can clear hotkey by pressing Backspace'],
    itemCheckFirst: ['Open item on wiki'],
    autoSearch: ['Perform an auto search, when pressing']
  };
  var ENGLISH = {
    clearHotkey: markers.clearHotkey[0],
    itemCheckFirst: markers.itemCheckFirst[0],
    autoSearch: markers.autoSearch[0]
  };
  var loadedLang = 'en';

  function loadLanguage() {
    var lang = document.documentElement.lang || 'en';
    if (lang === loadedLang) return;
    loadedLang = lang;
    if (lang === 'en') {
      for (var key in ENGLISH) markers[key] = [ENGLISH[key]];
      return;
    }
    fetch('data/' + encodeURIComponent(lang) + '/app_i18n.json')
      .then(function (r) { return r.json(); })
      .then(function (messages) {
        if (loadedLang !== lang) return; // switched again meanwhile
        for (var key in MARKER_KEYS) {
          var path = MARKER_KEYS[key];
          var section = messages[path[0]];
          var text = section && section[path[1]];
          markers[key] = text ? [ENGLISH[key], text] : [ENGLISH[key]];
        }
        addHotkeyUiInfo();
      })
      .catch(function () {});
  }

  function matches(key, text) {
    return markers[key].indexOf(text) !== -1;
  }

  function startsWithMarker(key, text) {
    return markers[key].some(function (m) { return text.indexOf(m) === 0; });
  }

  var BANNER_TEXT = 'Hotkeys are configured in KDE System Settings → Shortcuts.';

  function makeBanner() {
    var banner = document.createElement('div');
    banner.textContent = BANNER_TEXT;
    banner.style.cssText =
      'margin-bottom:0.75rem;padding:0.5rem;border-radius:0.25rem;' +
      'background:#7c2d12;color:#fed7aa;font-size:0.875rem;';
    return banner;
  }

  function addHotkeyUiInfo() {
    loadLanguage();
    // Hotkeys tab: the hint box + the rows container that follows it are
    // the only two children that are actually hotkey UI — the stash-scroll
    // radio (a real, unrelated setting) is a later sibling and stays put.
    var divs = document.querySelectorAll('div');
    for (var i = 0; i < divs.length; i++) {
      var marker = divs[i];
      if (marker.dataset.aptBannerAdded) continue;
      if (!matches('clearHotkey', marker.textContent.trim())) continue;
      var tabRoot = marker.parentElement;
      if (!tabRoot) continue;
      var rowsContainer = marker.nextElementSibling;
      tabRoot.insertBefore(makeBanner(), marker);
      marker.style.display = 'none';
      if (rowsContainer) rowsContainer.style.display = 'none';
      marker.dataset.aptBannerAdded = '1';
    }

    // Item info tab: its entire content is hotkey rows (no unrelated
    // settings mixed in), so every existing child of the tab root gets
    // hidden and the banner becomes the tab's only visible content.
    var labels = document.querySelectorAll('label');
    for (var j = 0; j < labels.length; j++) {
      if (!startsWithMarker('itemCheckFirst', labels[j].textContent.trim())) continue;
      var firstRow = labels[j].parentElement;
      var tabRoot2 = firstRow ? firstRow.parentElement : null;
      if (!tabRoot2 || tabRoot2.dataset.aptBannerAdded) continue;
      tabRoot2.dataset.aptBannerAdded = '1';
      var existingRows = Array.prototype.slice.call(tabRoot2.children);
      tabRoot2.insertBefore(makeBanner(), tabRoot2.firstChild);
      for (var r = 0; r < existingRows.length; r++) existingRows[r].style.display = 'none';
    }

    var searchDivs = document.querySelectorAll('div');
    for (var k = 0; k < searchDivs.length; k++) {
      var el = searchDivs[k];
      if (el.dataset.aptLabelsFixed || el.children.length > 0) continue;
      if (!matches('autoSearch', el.textContent.trim())) continue;
      el.dataset.aptLabelsFixed = '1';
      var container = el.nextElementSibling;
      if (!container) continue;
      // Source order matches Vue's v-if order: hotkeyQuick's span (if
      // rendered) always comes before hotkeyLocked's — same two terms
      // settings/hotkeys.vue itself uses for these ("Auto-hide Mode" /
      // "Open without auto-hide") so the label stays recognizable, just
      // without a specific (and potentially stale) key combo.
      var spans = container.querySelectorAll('span');
      var SPAN_LABELS = ['Auto-hide (KDE)', 'No auto-hide (KDE)'];
      for (var m = 0; m < spans.length; m++) {
        spans[m].textContent = SPAN_LABELS[m] || 'Set in KDE';
        spans[m].style.whiteSpace = 'nowrap';
      }
    }
  }
  // ponytail: whole-document MutationObserver rather than scoping to the
  // settings window container specifically — simplest thing that works,
  // scope tighter if it ever shows up as a real perf cost.
  new MutationObserver(addHotkeyUiInfo).observe(document.documentElement, {
    childList: true,
    subtree: true
  });
  addHotkeyUiInfo();
})();
