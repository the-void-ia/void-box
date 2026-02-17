(function () {
  function escapeHtml(text) {
    return text
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;');
  }

  function highlightRust(codeEl) {
    var raw = codeEl.textContent || '';
    var src = escapeHtml(raw);
    var stash = [];

    function token(value, kind) {
      stash.push('<span class="' + kind + '">' + value + '</span>');
      // Use private-use unicode markers so later regex passes cannot corrupt placeholders.
      return String.fromCharCode(0xe000 + (stash.length - 1));
    }

    src = src.replace(/\/\/.*$/gm, function (m) { return token(m, 'tok-com'); });
    src = src.replace(/"(?:\\.|[^"\\])*"/g, function (m) { return token(m, 'tok-str'); });
    src = src.replace(/\b(?:use|let|mut|pub|struct|enum|impl|fn|trait|for|in|if|else|match|return|async|await|true|false)\b/g, function (m) {
      return token(m, 'tok-kw');
    });
    src = src.replace(/\b(?:VoidBox|Skill|LlmProvider|Network|Pipeline|None|Result|String)\b/g, function (m) { return token(m, 'tok-ty'); });
    src = src.replace(/\b\d+\b/g, function (m) { return token(m, 'tok-num'); });
    src = src.replace(/\b([a-zA-Z_][a-zA-Z0-9_]*)(?=\s*\()/g, function (m) { return token(m, 'tok-fn'); });

    src = src.replace(/[\ue000-\uf8ff]/g, function (marker) {
      return stash[marker.charCodeAt(0) - 0xe000] || marker;
    });

    codeEl.innerHTML = src;
  }

  function highlightYaml(codeEl) {
    var raw = codeEl.textContent || '';
    var src = escapeHtml(raw);
    var stash = [];

    function token(value, kind) {
      stash.push('<span class="' + kind + '">' + value + '</span>');
      return String.fromCharCode(0xe000 + (stash.length - 1));
    }

    src = src.replace(/#.*$/gm, function (m) { return token(m, 'tok-com'); });
    src = src.replace(/"(?:\\.|[^"\\])*"/g, function (m) { return token(m, 'tok-str'); });
    src = src.replace(/^(\s*)([\w_][\w_-]*)(\s*:)/gm, function (m, ws, key, colon) {
      return ws + token(key, 'tok-kw') + colon;
    });
    src = src.replace(/\b\d+\b/g, function (m) { return token(m, 'tok-num'); });
    src = src.replace(/\b(?:true|false|null)\b/g, function (m) { return token(m, 'tok-ty'); });

    src = src.replace(/[\ue000-\uf8ff]/g, function (marker) {
      return stash[marker.charCodeAt(0) - 0xe000] || marker;
    });

    codeEl.innerHTML = src;
  }

  var rustBlocks = document.querySelectorAll('code.language-rust');
  for (var b = 0; b < rustBlocks.length; b++) {
    highlightRust(rustBlocks[b]);
  }

  var yamlBlocks = document.querySelectorAll('code.language-yaml');
  for (var b = 0; b < yamlBlocks.length; b++) {
    highlightYaml(yamlBlocks[b]);
  }

  // Code tab switching
  var tabs = document.querySelectorAll('.code-tab');
  for (var t = 0; t < tabs.length; t++) {
    tabs[t].addEventListener('click', function () {
      var target = this.getAttribute('data-tab');
      var siblings = this.parentNode.querySelectorAll('.code-tab');
      for (var s = 0; s < siblings.length; s++) {
        siblings[s].classList.remove('active');
      }
      this.classList.add('active');
      var parent = this.parentNode.parentNode;
      var panels = parent.querySelectorAll('.code-wrap[data-panel]');
      for (var p = 0; p < panels.length; p++) {
        panels[p].style.display = panels[p].getAttribute('data-panel') === target ? '' : 'none';
      }
    });
  }

  var links = document.querySelectorAll('a[href^="#"]');
  for (var i = 0; i < links.length; i++) {
    links[i].addEventListener('click', function (e) {
      var id = this.getAttribute('href');
      if (!id || id.length < 2) return;
      var target = document.querySelector(id);
      if (!target) return;
      e.preventDefault();
      target.scrollIntoView({ behavior: 'smooth', block: 'start' });
      history.replaceState(null, '', id);
    });
  }
})();
