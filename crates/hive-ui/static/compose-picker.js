// hive-ui compose-time picker
//
// Watches the body textarea for `#task`, `#note`, `#event`, `#journal`,
// `#person`, `#ai` triggers and opens a typeahead dropdown that fetches
// from /api/recent. On selection, replaces `#<type><query>` with the
// canonical anchor form: `[[type:uuid|title]]`.
//
// Vanilla JS only, no framework, no build step.

(function () {
    'use strict';

    var TYPES = ['task', 'note', 'event', 'journal', 'person', 'ai'];
    var DEBOUNCE_MS = 250;
    var MAX_ROWS = 20;

    var textarea = document.querySelector('textarea[name="body"]');
    if (!textarea) return;

    var dropdown = null;
    var rows = [];           // current result set [{id, title, meta}]
    var activeIdx = 0;
    var triggerStart = -1;   // index in textarea.value where `#` lives
    var triggerType = null;
    var fetchTimer = null;
    var fetchSeq = 0;        // guards against stale responses

    function close() {
        if (dropdown) {
            dropdown.remove();
            dropdown = null;
        }
        rows = [];
        activeIdx = 0;
        triggerStart = -1;
        triggerType = null;
        if (fetchTimer) {
            clearTimeout(fetchTimer);
            fetchTimer = null;
        }
    }

    function isOpen() {
        return dropdown !== null;
    }

    // Scan back from cursor to find an unfinished `#type` (where `type` is
    // one of the known types) that's preceded by whitespace or start. If
    // found, returns { type, queryStart, queryEnd, query }. Otherwise null.
    function detectTrigger() {
        var pos = textarea.selectionStart;
        if (pos !== textarea.selectionEnd) return null;
        var text = textarea.value;
        // Walk back from cursor to the most recent `#`. Bail on whitespace
        // (means there's no current `#` token under the cursor).
        var i = pos - 1;
        while (i >= 0) {
            var c = text.charAt(i);
            if (c === '#') break;
            if (/\s/.test(c)) return null;
            i--;
        }
        if (i < 0) return null;
        // `#` must be at start-of-textarea or preceded by whitespace.
        if (i > 0 && !/\s/.test(text.charAt(i - 1))) return null;
        // Match `#<word>` where word is letters only (so the type fragment).
        // Cursor sits at pos; the `#` is at i; the word runs from i+1..pos.
        var word = text.substring(i + 1, pos);
        // Only fire while the word is a prefix of a known type, OR the word
        // starts with a known type followed by a query (`#task foo` ... but
        // we don't allow spaces inside the trigger ... use the prefix form).
        // Pattern: `#type` exactly, or `#type<rest-no-space>` where rest
        // becomes the query.
        // Strategy: find the longest known type that's a prefix of word.
        var matchedType = null;
        for (var t = 0; t < TYPES.length; t++) {
            if (word === TYPES[t]) {
                matchedType = TYPES[t];
                break;
            }
        }
        if (!matchedType) {
            // Still typing the type? `#tas`, `#jo`, etc. ... open with no
            // query yet so users see a quick hint of recent items.
            for (var p = 0; p < TYPES.length; p++) {
                if (TYPES[p].indexOf(word) === 0 && word.length > 0) {
                    // Don't open yet ... we only fire on a complete type.
                    // Keep this branch as a hook for future "preview" mode.
                    return null;
                }
            }
            // Past the type ... maybe `#task<query>`?
            for (var q = 0; q < TYPES.length; q++) {
                var ty = TYPES[q];
                if (word.length > ty.length && word.indexOf(ty) === 0) {
                    matchedType = ty;
                    break;
                }
            }
            if (!matchedType) return null;
        }
        var query = word.substring(matchedType.length);
        return {
            type: matchedType,
            triggerIdx: i,
            queryEnd: pos,
            query: query,
        };
    }

    function scheduleFetch(type, query) {
        if (fetchTimer) clearTimeout(fetchTimer);
        fetchTimer = setTimeout(function () {
            doFetch(type, query);
        }, DEBOUNCE_MS);
    }

    function doFetch(type, query) {
        var seq = ++fetchSeq;
        var url = '/api/recent?type=' + encodeURIComponent(type) +
            '&q=' + encodeURIComponent(query);
        fetch(url, { credentials: 'same-origin' })
            .then(function (resp) {
                if (!resp.ok) throw new Error('HTTP ' + resp.status);
                return resp.json();
            })
            .then(function (data) {
                if (seq !== fetchSeq) return;       // stale
                if (!isOpen()) return;              // user dismissed
                rows = (data || []).slice(0, MAX_ROWS);
                activeIdx = 0;
                render();
            })
            .catch(function () {
                if (seq !== fetchSeq) return;
                if (!isOpen()) return;
                rows = [];
                render();
            });
    }

    function ensureDropdown() {
        if (dropdown) return dropdown;
        dropdown = document.createElement('div');
        dropdown.className = 'picker-dropdown';
        dropdown.setAttribute('role', 'listbox');
        document.body.appendChild(dropdown);
        position();
        return dropdown;
    }

    // Position the dropdown directly under the textarea. We don't try to
    // hover near the caret pixel-perfectly (caret pixel math in a
    // textarea is painful and the dropdown is small anyway). Under the
    // textarea, full-width is good enough and predictable.
    function position() {
        if (!dropdown) return;
        var rect = textarea.getBoundingClientRect();
        dropdown.style.position = 'absolute';
        dropdown.style.top = (window.scrollY + rect.bottom + 4) + 'px';
        dropdown.style.left = (window.scrollX + rect.left) + 'px';
    }

    function render() {
        if (!dropdown) return;
        if (rows.length === 0) {
            dropdown.innerHTML = '<div class="picker-row picker-empty">no matches</div>';
            return;
        }
        var html = '';
        for (var i = 0; i < rows.length; i++) {
            var r = rows[i];
            var cls = 'picker-row' + (i === activeIdx ? ' active' : '');
            html += '<div class="' + cls + '" data-idx="' + i + '" role="option">' +
                '<div class="picker-title">' + escapeHtml(r.title || '(untitled)') + '</div>';
            if (r.meta) {
                html += '<div class="picker-meta">' + escapeHtml(r.meta) + '</div>';
            }
            html += '</div>';
        }
        dropdown.innerHTML = html;
        // Wire row clicks.
        var nodes = dropdown.querySelectorAll('.picker-row[data-idx]');
        for (var n = 0; n < nodes.length; n++) {
            nodes[n].addEventListener('mousedown', function (e) {
                // mousedown (not click) so the textarea doesn't blur first
                // and lose its selection.
                e.preventDefault();
                var idx = parseInt(this.getAttribute('data-idx'), 10);
                if (!isNaN(idx)) selectRow(idx);
            });
            nodes[n].addEventListener('mouseenter', function () {
                var idx = parseInt(this.getAttribute('data-idx'), 10);
                if (!isNaN(idx)) {
                    activeIdx = idx;
                    render();
                }
            });
        }
    }

    function selectRow(idx) {
        if (idx < 0 || idx >= rows.length) return;
        var r = rows[idx];
        if (!r || !r.id) return;
        var anchor = '[[' + triggerType + ':' + r.id + '|' + r.title + ']]';
        var v = textarea.value;
        var before = v.substring(0, triggerStart);
        var after = v.substring(/* queryEnd snapshot */ getQueryEnd());
        textarea.value = before + anchor + after;
        var newPos = before.length + anchor.length;
        textarea.selectionStart = textarea.selectionEnd = newPos;
        textarea.focus();
        close();
        // Fire input so any other listeners see the new value.
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
    }

    // We snapshot triggerEnd at trigger-detect time, but the textarea
    // could have grown if the user typed during the fetch. Recompute by
    // re-running the detector ... if it matches the same trigger, use
    // its current end; else fall back to the original snapshot.
    var queryEndSnapshot = -1;
    function setQueryEnd(end) { queryEndSnapshot = end; }
    function getQueryEnd() {
        var cur = detectTrigger();
        if (cur && cur.triggerIdx === triggerStart && cur.type === triggerType) {
            return cur.queryEnd;
        }
        return queryEndSnapshot;
    }

    function onInput() {
        var trig = detectTrigger();
        if (!trig) {
            if (isOpen()) close();
            return;
        }
        // First time we see this trigger? Open the dropdown.
        if (!isOpen() || trig.triggerIdx !== triggerStart || trig.type !== triggerType) {
            triggerStart = trig.triggerIdx;
            triggerType = trig.type;
            ensureDropdown();
            rows = [];
            activeIdx = 0;
            render();
        }
        setQueryEnd(trig.queryEnd);
        scheduleFetch(trig.type, trig.query);
    }

    function onKeydown(e) {
        if (!isOpen()) return;
        switch (e.key) {
            case 'ArrowDown':
                e.preventDefault();
                if (rows.length > 0) {
                    activeIdx = (activeIdx + 1) % rows.length;
                    render();
                }
                break;
            case 'ArrowUp':
                e.preventDefault();
                if (rows.length > 0) {
                    activeIdx = (activeIdx - 1 + rows.length) % rows.length;
                    render();
                }
                break;
            case 'Enter':
            case 'Tab':
                if (rows.length > 0) {
                    e.preventDefault();
                    selectRow(activeIdx);
                }
                break;
            case 'Escape':
                e.preventDefault();
                close();
                break;
        }
    }

    function onClickAway(e) {
        if (!isOpen()) return;
        if (dropdown && (dropdown === e.target || dropdown.contains(e.target))) return;
        if (e.target === textarea) return;
        close();
    }

    function onScroll() {
        if (isOpen()) position();
    }

    function escapeHtml(s) {
        return String(s)
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;')
            .replace(/"/g, '&quot;')
            .replace(/'/g, '&#39;');
    }

    textarea.addEventListener('input', onInput);
    textarea.addEventListener('keydown', onKeydown);
    // Caret moves via arrow keys / mouse-click can also expose / hide a
    // trigger without an `input` event. Re-detect on keyup + click.
    textarea.addEventListener('keyup', function (e) {
        // ArrowDown/Up are consumed by the dropdown; skip them here.
        if (e.key === 'ArrowDown' || e.key === 'ArrowUp' ||
            e.key === 'Enter' || e.key === 'Escape' || e.key === 'Tab') return;
        onInput();
    });
    textarea.addEventListener('click', onInput);
    textarea.addEventListener('blur', function () {
        // Delay close so a row-mousedown can win the race.
        setTimeout(function () {
            if (document.activeElement !== textarea) close();
        }, 150);
    });
    document.addEventListener('mousedown', onClickAway);
    window.addEventListener('scroll', onScroll, true);
    window.addEventListener('resize', onScroll);
})();
