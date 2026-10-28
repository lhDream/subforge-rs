/* Frost 1.0 — 原生 DOM 增强。放在 head 中样式前；DOM 就绪后显式调用 Frost.init(root)。 */
(() => {
  "use strict";
  if (window.Frost && window.Frost.version === "1.0.0") return;

  const instances = new WeakMap();
  const themeSubscribers = new Set();
  const themeKey = "web-standards.preferences.v3";
  const themeValues = ["light", "dark", "system"];
  const media = query => typeof window.matchMedia === "function" ? window.matchMedia(query) : {
    matches: false, addEventListener() {}, removeEventListener() {}
  };
  const darkMedia = media("(prefers-color-scheme: dark)");
  const forcedMedia = media("(forced-colors: active)");
  let preference = "system";
  let storageAvailable = true;
  let mediaListening = false;
  let sequence = 0;

  try {
    for (const key of [themeKey, "web-standards.preferences.v2", "web-standards.preferences.v1"]) {
      const raw = window.localStorage.getItem(key);
      if (!raw) continue;
      let saved;
      try { saved = JSON.parse(raw); } catch { continue; }
      if (saved && themeValues.includes(saved.theme)) { preference = saved.theme; break; }
    }
  } catch { storageAvailable = false; }

  function effectiveTheme() { return preference === "system" ? (darkMedia.matches ? "dark" : "light") : preference; }
  function renderTheme() {
    const theme = effectiveTheme();
    document.documentElement.dataset.theme = theme;
    document.documentElement.style.colorScheme = theme;
    themeSubscribers.forEach(render => render());
  }
  function setTheme(value) {
    if (!themeValues.includes(value)) return;
    preference = value;
    try { window.localStorage.setItem(themeKey, JSON.stringify({ theme: preference })); storageAvailable = true; }
    catch { storageAvailable = false; }
    renderTheme();
  }
  function listenMedia(add) {
    if (mediaListening === add) return;
    mediaListening = add;
    [darkMedia, forcedMedia].forEach(query => {
      if (query.addEventListener) query[add ? "addEventListener" : "removeEventListener"]("change", renderTheme);
      else if (query.addListener) query[add ? "addListener" : "removeListener"](renderTheme);
    });
  }
  renderTheme();

  const isDisabled = node => Boolean(node.disabled || node.matches(":disabled") || node.getAttribute("aria-disabled") === "true");
  const focusableSelector = "button, [href], input, select, textarea, summary, [contenteditable='true'], audio[controls], video[controls], [tabindex]";
  function safeFocus(node) {
    if (node && node.isConnected && !isDisabled(node) && typeof node.focus === "function") node.focus({ preventScroll: true });
  }
  function isVisible(node) {
    return !node.closest("[hidden], [inert]") && node.getClientRects().length > 0 && getComputedStyle(node).visibility !== "hidden";
  }
  function availableFocusables(container) {
    return Array.from(container.querySelectorAll(focusableSelector)).filter(node => !isDisabled(node) && node.tabIndex >= 0 && isVisible(node));
  }

  function init(root) {
    if (!(root instanceof window.Element) || !root.hasAttribute("data-frost-root")) {
      throw new TypeError("Frost.init(root) 需要带 data-frost-root 的 DOM 元素。");
    }
    if (instances.has(root)) return instances.get(root);

    let destroyed = false;
    let portal = null;
    let toastStack = null;
    let activeMenu = null;
    let activeDialog = null;
    let flowObserver = null;
    const flowObserverOptions = { subtree: true, childList: true, attributes: true, attributeFilter: ["disabled", "aria-disabled", "readonly", "aria-invalid", "hidden", "inert", "open"] };
    const disposers = [];
    const toastRecords = new Set();
    const instanceId = ++sequence;

    function belongs(node) {
      if (!(node instanceof window.Element)) return false;
      const owner = node.closest("[data-frost-root]");
      return owner === root || (portal !== null && owner === portal);
    }
    function ownedAll(selector) {
      return [root, portal].filter(Boolean).flatMap(scope => Array.from(scope.querySelectorAll(selector))).filter(belongs);
    }
    function byId(id) { return id ? ownedAll("[id]").find(node => node.id === id) || null : null; }
    function on(target, event, callback, options) {
      target.addEventListener(event, callback, options);
      disposers.push(() => target.removeEventListener(event, callback, options));
    }
    function initFlow() {
      const css = window.CSS;
      const supported = css && typeof css.supports === "function" &&
        css.supports("background", "conic-gradient(from 0deg in srgb, red, blue)") &&
        css.supports("selector(:has(*))") &&
        (css.supports("mask-composite", "exclude") || css.supports("-webkit-mask-composite", "xor"));
      // 不支持增强或缺少帧 API 时只保留静态 CSS，不创建计时器替代品。
      if (!supported || typeof window.requestAnimationFrame !== "function" || typeof window.cancelAnimationFrame !== "function") return;
      const reducedMedia = media("(prefers-reduced-motion: reduce)");
      const records = new Map();
      let frame = null;
      let active = null;
      const editable = shell => {
        const control = Array.from(shell.children).find(node => node.matches(".control"));
        return shell.isConnected && belongs(shell) && shell.contains(document.activeElement) &&
          !shell.closest("[hidden], [inert]") && control && !isDisabled(control) &&
          !control.hasAttribute("readonly") && control.getAttribute("aria-invalid") !== "true";
      };
      function stop() {
        if (frame !== null) window.cancelAnimationFrame(frame);
        frame = null;
        active = null;
      }
      function restoreAngle(record, shell) {
        if (record.value) shell.style.setProperty("--frost-flow-angle", record.value, record.priority);
        else shell.style.removeProperty("--frost-flow-angle");
        if (!record.hadStyle && !shell.style.length) shell.removeAttribute("style");
      }
      function advance(now) {
        frame = null;
        if (destroyed || !active || document.hidden || reducedMedia.matches || forcedMedia.matches || !editable(active.shell)) { stop(); return; }
        const { shell, record, duration, previous } = active;
        record.angle = (record.angle + Math.max(0, now - previous) * 360 / duration) % 360;
        active.previous = now;
        // 直接写数值；伪元素继承此值，无第二条 CSS 动画或 @property 插值。
        shell.style.setProperty("--frost-flow-angle", `${record.angle.toFixed(3)}deg`, record.priority);
        frame = window.requestAnimationFrame(advance);
      }
      function refresh() {
        const focused = document.activeElement;
        const shell = focused instanceof window.Element ? focused.closest(".control-shell") : null;
        if (destroyed || document.hidden || reducedMedia.matches || forcedMedia.matches || !shell || !editable(shell)) { stop(); return; }
        if (active?.shell === shell) return;
        stop();
        const style = getComputedStyle(shell);
        if (!records.has(shell)) records.set(shell, {
          angle: Number.parseFloat(style.getPropertyValue("--frost-flow-angle")) || 0,
          value: shell.style.getPropertyValue("--frost-flow-angle"),
          priority: shell.style.getPropertyPriority("--frost-flow-angle"),
          hadStyle: shell.hasAttribute("style")
        });
        const time = style.getPropertyValue("--flow-duration").trim().match(/^(\d*\.?\d+)(ms|s)$/);
        const duration = time && Number(time[1]) > 0 ? Number(time[1]) * (time[2] === "s" ? 1000 : 1) : 2800;
        active = { shell, record: records.get(shell), duration, previous: performance.now() };
        frame = window.requestAnimationFrame(advance);
      }
      on(document, "focusin", refresh);
      on(document, "focusout", event => { if (active && !active.shell.contains(event.relatedTarget)) stop(); });
      on(document, "visibilitychange", refresh);
      [reducedMedia, forcedMedia].forEach(query => {
        if (query.addEventListener) on(query, "change", refresh);
        else if (query.addListener) {
          query.addListener(refresh);
          disposers.push(() => query.removeListener(refresh));
        }
      });
      // 属性变化后重新检查焦点，支持校验修复、fieldset 禁用以及动态插入。
      if (typeof window.MutationObserver === "function") {
        flowObserver = new window.MutationObserver(changes => {
          if (changes.some(change => change.type === "childList")) records.forEach((record, shell) => {
            // 长期存在的页面根不保留已卸载字段；移动到本实例 portal 则继续保留。
            if (!shell.isConnected || !belongs(shell)) {
              restoreAngle(record, shell);
              records.delete(shell);
            }
          });
          refresh();
        });
        [root, portal].filter(Boolean).forEach(scope => flowObserver.observe(scope, flowObserverOptions));
      }
      disposers.push(() => {
        stop();
        flowObserver?.disconnect();
        flowObserver = null;
        records.forEach(restoreAngle);
        records.clear();
      });
      refresh();
    }
    function delegate(event) {
      const target = event.target instanceof window.Element ? event.target : event.target.parentElement;
      if (!target || target.closest("[data-frost-root]") !== event.currentTarget) return;
      if (event.type === "click") handleClick(event, target);
      if (event.type === "keydown") handleKeydown(event, target);
    }
    function ensurePortal() {
      if (portal) return portal;
      portal = document.createElement("div");
      portal.className = "frost-portal";
      portal.setAttribute("data-frost-root", "");
      portal.setAttribute("data-frost-portal", String(instanceId));
      document.body.append(portal);
      on(portal, "click", delegate);
      on(portal, "keydown", delegate);
      flowObserver?.observe(portal, flowObserverOptions);
      return portal;
    }
    function updateThemeUI() {
      const theme = effectiveTheme();
      ownedAll("[data-theme-choice]").forEach(button => {
        button.setAttribute("aria-pressed", String(button.dataset.themeChoice === preference));
      });
      const labels = [
        ["[data-frost-theme-state], #theme-state", `${theme === "dark" ? "午夜深色" : "晨雾浅色"}${preference === "system" ? " · 跟随系统" : ""}`],
        ["[data-frost-storage-state], #storage-state", storageAvailable ? "主题选择自动保存在当前浏览器。" : "存储不可用，主题仅在当前页面生效。"]
      ];
      const supports = window.CSS && (CSS.supports("backdrop-filter", "blur(1px)") || CSS.supports("-webkit-backdrop-filter", "blur(1px)"));
      labels.push(["[data-frost-material-state], #material-state", forcedMedia.matches ? "系统高对比度" : supports ? "FROSTED GLASS" : "浏览器兼容显示"]);
      labels.forEach(([selector, value]) => ownedAll(selector).forEach(node => { node.textContent = value; }));
    }

    function tabsFor(group) {
      const list = Array.from(group.querySelectorAll('[role="tablist"]')).find(node => node.closest("[data-frost-tabs]") === group);
      if (!list) return [];
      return Array.from(list.querySelectorAll('[role="tab"]')).filter(node => node.closest('[role="tablist"]') === list && belongs(node));
    }
    function activateTab(tab, focus = false) {
      const group = tab.closest("[data-frost-tabs]");
      if (!group || isDisabled(tab)) return;
      const tabs = tabsFor(group);
      if (!tabs.includes(tab)) return;
      const panel = byId(tab.getAttribute("aria-controls"));
      if (!panel || panel.closest("[data-frost-tabs]") !== group) return;
      tabs.forEach(item => {
        const selected = item === tab;
        item.setAttribute("aria-selected", String(selected));
        item.tabIndex = selected ? 0 : -1;
        const target = byId(item.getAttribute("aria-controls"));
        if (target && target.closest("[data-frost-tabs]") === group) target.hidden = !selected;
      });
      if (focus) safeFocus(tab);
    }
    ownedAll("[data-frost-tabs]").forEach(group => {
      const tabs = tabsFor(group);
      const valid = tabs.filter(tab => !isDisabled(tab) && byId(tab.getAttribute("aria-controls"))?.closest("[data-frost-tabs]") === group);
      const selected = valid.find(tab => tab.getAttribute("aria-selected") === "true") || valid[0];
      if (selected) activateTab(selected);
      else tabs.forEach(tab => { tab.tabIndex = -1; tab.setAttribute("aria-selected", "false"); });
    });

    function moveToPortal(node) {
      const marker = document.createComment("frost: original position");
      node.before(marker);
      ensurePortal().append(node);
      return () => {
        if (marker.parentNode) marker.replaceWith(node);
        else node.remove();
      };
    }
    function menuItems(menu) {
      return Array.from(menu.querySelectorAll('[role="menuitem"]')).filter(item => !isDisabled(item) && !item.hidden && item.closest('[role="menu"]') === menu);
    }
    function positionMenu() {
      if (!activeMenu) return;
      const { node, trigger, minimumWidth } = activeMenu;
      if (!trigger.isConnected) { closeMenu(false); return; }
      const margin = 8;
      const gap = 8;
      const viewportWidth = document.documentElement.clientWidth || window.innerWidth;
      const viewportHeight = window.innerHeight;
      const anchor = trigger.getBoundingClientRect();
      node.style.position = "fixed";
      // min-width 优先于 max-width；在窄窗口或高缩放时同步收紧最小宽度。
      node.style.minWidth = `${Math.min(minimumWidth, Math.max(0, viewportWidth - margin * 2))}px`;
      node.style.maxWidth = `${Math.max(0, viewportWidth - margin * 2)}px`;
      node.style.maxHeight = `${Math.max(0, viewportHeight - margin * 2)}px`;
      node.style.overflowY = "auto";
      const size = node.getBoundingClientRect();
      const below = viewportHeight - anchor.bottom - gap - margin;
      const above = anchor.top - gap - margin;
      const openAbove = size.height > below && above > below;
      const availableHeight = Math.max(0, openAbove ? above : below);
      node.style.maxHeight = `${availableHeight}px`;
      const height = Math.min(size.height, availableHeight);
      const top = openAbove ? anchor.top - gap - height : anchor.bottom + gap;
      const rtl = getComputedStyle(trigger).direction === "rtl";
      const desiredLeft = rtl ? anchor.right - size.width : anchor.left;
      node.style.left = `${Math.max(margin, Math.min(desiredLeft, viewportWidth - size.width - margin))}px`;
      node.style.top = `${Math.max(margin, Math.min(top, viewportHeight - height - margin))}px`;
    }
    function closeMenu(restoreFocus = true) {
      const entry = activeMenu;
      if (!entry) return;
      activeMenu = null;
      entry.node.hidden = true;
      entry.trigger.setAttribute("aria-expanded", "false");
      if (entry.style === null) entry.node.removeAttribute("style");
      else entry.node.setAttribute("style", entry.style);
      entry.restore();
      window.removeEventListener("resize", positionMenu);
      window.removeEventListener("scroll", positionMenu, true);
      if (restoreFocus) safeFocus(entry.trigger);
    }
    function openMenu(trigger, last = false) {
      const menu = byId(trigger.dataset.frostMenu);
      if (!menu || menu.getAttribute("role") !== "menu" || isDisabled(trigger)) return;
      if (activeMenu?.node === menu) { closeMenu(); return; }
      if (document.querySelector("dialog[open]")) return;
      closeMenu(false);
      trigger.setAttribute("aria-haspopup", "menu");
      trigger.setAttribute("aria-expanded", "true");
      const entry = {
        node: menu, trigger, style: menu.getAttribute("style"),
        minimumWidth: Number.parseFloat(getComputedStyle(menu).minWidth) || 0,
        restore: moveToPortal(menu)
      };
      activeMenu = entry;
      menu.hidden = false;
      positionMenu();
      window.addEventListener("resize", positionMenu);
      window.addEventListener("scroll", positionMenu, true);
      const items = menuItems(menu);
      safeFocus(last ? items.at(-1) : items[0]);
    }
    function selectMenuItem(item) {
      const entry = activeMenu;
      if (!entry || !entry.node.contains(item) || isDisabled(item)) return;
      const detail = { value: item.dataset.frostValue || "", trigger: entry.trigger, menu: entry.node };
      closeMenu();
      root.dispatchEvent(new CustomEvent("frost:menu-select", { detail, bubbles: true }));
    }
    function leaveMenu(backwards) {
      const trigger = activeMenu?.trigger;
      closeMenu(false);
      if (!trigger) return;
      const candidates = availableFocusables(document);
      const index = candidates.indexOf(trigger);
      safeFocus(candidates[index + (backwards ? -1 : 1)] || trigger);
    }

    function finishDialog(entry, restoreFocus = true) {
      if (activeDialog !== entry) return;
      activeDialog = null;
      entry.node.removeEventListener("close", entry.onClose);
      entry.node.removeEventListener("cancel", entry.onCancel);
      entry.restore();
      if (restoreFocus) safeFocus(entry.trigger);
    }
    function closeDialog(restoreFocus = true) {
      const entry = activeDialog;
      if (!entry) return;
      if (entry.node.open) entry.node.close();
      finishDialog(entry, restoreFocus);
    }
    function openDialog(trigger) {
      const dialog = byId(trigger.dataset.frostDialog);
      if (!dialog || dialog.tagName !== "DIALOG" || isDisabled(trigger)) return;
      if (activeDialog?.node === dialog) return;
      if (document.querySelector("dialog[open]")) return;
      closeMenu(false);
      const entry = { node: dialog, trigger, restore: moveToPortal(dialog), onClose: null, onCancel: null };
      entry.onClose = () => finishDialog(entry);
      entry.onCancel = event => { event.preventDefault(); closeDialog(); };
      activeDialog = entry;
      dialog.addEventListener("close", entry.onClose);
      dialog.addEventListener("cancel", entry.onCancel);
      try {
        dialog.showModal();
        const autofocus = dialog.querySelector("[autofocus]");
        const first = autofocus && !isDisabled(autofocus) ? autofocus : availableFocusables(dialog)[0];
        if (first) safeFocus(first);
      } catch {
        finishDialog(entry);
        toast("当前环境无法打开对话框。请使用支持原生 dialog 的浏览器。", { tone: "error" });
      }
    }

    function handleClick(event, target) {
      if (destroyed || event.defaultPrevented) return;
      const interactive = target.closest("button, a, input, [role='tab'], [role='menuitem']");
      if (interactive && belongs(interactive) && isDisabled(interactive)) { event.preventDefault(); return; }
      const choice = target.closest("[data-theme-choice]");
      if (choice && belongs(choice)) { event.preventDefault(); setTheme(choice.dataset.themeChoice); return; }
      const clear = target.closest("[data-frost-clear]");
      const password = target.closest("[data-frost-password]");
      if (clear || password) {
        const action = clear || password;
        if (!belongs(action)) return;
        const control = action.closest(".control-shell")?.querySelector("input, textarea");
        if (!control || !belongs(control) || control.disabled || control.readOnly) return;
        event.preventDefault();
        if (clear) {
          // 调用原生 setter，确保 React 等受控输入也能收到随后派发的 input 变化。
          const prototype = control instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
          const setter = Object.getOwnPropertyDescriptor(prototype, "value")?.set;
          if (setter) setter.call(control, "");
          else control.value = "";
          control.dispatchEvent(new Event("input", { bubbles: true }));
          control.dispatchEvent(new Event("change", { bubbles: true }));
          safeFocus(control);
        } else if (control instanceof HTMLInputElement && ["password", "text"].includes(control.type)) {
          const show = control.type === "password";
          control.type = show ? "text" : "password";
          action.setAttribute("aria-pressed", String(show));
          action.setAttribute("aria-label", show ? "隐藏密码" : "显示密码");
          const caption = action.querySelector("[data-frost-password-label]");
          if (caption) caption.textContent = show ? "隐藏" : "显示";
        }
        return;
      }
      const tab = target.closest('[role="tab"]');
      if (tab && belongs(tab)) { event.preventDefault(); activateTab(tab); return; }
      const menuTrigger = target.closest("[data-frost-menu]");
      if (menuTrigger && belongs(menuTrigger)) { event.preventDefault(); openMenu(menuTrigger); return; }
      const item = target.closest('[role="menuitem"]');
      if (item && belongs(item)) { event.preventDefault(); selectMenuItem(item); return; }
      const dialogTrigger = target.closest("[data-frost-dialog]");
      if (dialogTrigger && belongs(dialogTrigger)) { event.preventDefault(); openDialog(dialogTrigger); return; }
      const close = target.closest("[data-frost-close]");
      if (close && activeDialog && activeDialog.node.contains(close)) { event.preventDefault(); closeDialog(); }
    }
    function handleKeydown(event, target) {
      if (destroyed || event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return;
      const tab = target.closest('[role="tab"]');
      if (tab && belongs(tab)) {
        const group = tab.closest("[data-frost-tabs]");
        if (!group) return;
        const tabs = tabsFor(group).filter(item => !isDisabled(item) && byId(item.getAttribute("aria-controls"))?.closest("[data-frost-tabs]") === group);
        const index = tabs.indexOf(tab);
        const vertical = tab.closest('[role="tablist"]')?.getAttribute("aria-orientation") === "vertical";
        const rtl = getComputedStyle(tab).direction === "rtl";
        const previous = vertical ? "ArrowUp" : rtl ? "ArrowRight" : "ArrowLeft";
        const next = vertical ? "ArrowDown" : rtl ? "ArrowLeft" : "ArrowRight";
        let destination;
        if (event.key === "Home") destination = tabs[0];
        if (event.key === "End") destination = tabs.at(-1);
        if (event.key === previous) destination = tabs[(index - 1 + tabs.length) % tabs.length];
        if (event.key === next) destination = tabs[(index + 1) % tabs.length];
        if (destination) { event.preventDefault(); activateTab(destination, true); }
        return;
      }
      const trigger = target.closest("[data-frost-menu]");
      if (activeMenu && trigger === activeMenu.trigger && event.key === "Escape") {
        event.preventDefault(); closeMenu(); return;
      }
      if (trigger && belongs(trigger) && ["ArrowDown", "ArrowUp"].includes(event.key)) {
        event.preventDefault();
        if (activeMenu?.trigger === trigger) safeFocus(event.key === "ArrowUp" ? menuItems(activeMenu.node).at(-1) : menuItems(activeMenu.node)[0]);
        else openMenu(trigger, event.key === "ArrowUp");
        return;
      }
      if (activeMenu && activeMenu.node.contains(target)) {
        const items = menuItems(activeMenu.node);
        const index = items.indexOf(target.closest('[role="menuitem"]'));
        let destination;
        if (event.key === "ArrowDown") destination = items[(index + 1) % items.length];
        if (event.key === "ArrowUp") destination = index < 0 ? items.at(-1) : items[(index - 1 + items.length) % items.length];
        if (event.key === "Home") destination = items[0];
        if (event.key === "End") destination = items.at(-1);
        if (destination) { event.preventDefault(); safeFocus(destination); }
        else if (event.key === "Escape") { event.preventDefault(); closeMenu(); }
        else if (event.key === "Tab") { event.preventDefault(); leaveMenu(event.shiftKey); }
      }
    }

    function toast(message, options = {}) {
      if (destroyed) return () => {};
      const tone = ["info", "success", "error"].includes(options.tone) ? options.tone : "info";
      const action = options.action && typeof options.action.onClick === "function" ? options.action : null;
      if (!toastStack) {
        toastStack = document.createElement("div");
        toastStack.className = "toast-stack";
        toastStack.setAttribute("aria-label", "通知");
        ensurePortal().append(toastStack);
      }
      const node = document.createElement("div");
      node.className = "toast glass";
      node.dataset.tone = tone;
      node.setAttribute("role", tone === "error" ? "alert" : "status");
      node.setAttribute("aria-atomic", "true");
      const text = document.createElement("div");
      text.className = "toast-message";
      const toneLabels = { info: "提示", success: "成功", error: "错误" };
      text.textContent = `${toneLabels[tone]}：${String(message)}`;
      const actions = document.createElement("div");
      actions.className = "toast-actions";
      const close = document.createElement("button");
      close.className = "button button--icon toast-close";
      close.type = "button";
      close.setAttribute("aria-label", "关闭通知");
      close.textContent = "×";
      const requestedDuration = Number(options.duration ?? 5000);
      const record = {
        node, timer: null, started: 0, remaining: Number.isFinite(requestedDuration) ? Math.max(0, requestedDuration) : 5000,
        persistent: tone === "error" || Boolean(action), pauses: new Set(), disposed: false,
        previousFocus: document.activeElement, cleanup: []
      };
      const toastOn = (element, type, handler) => {
        element.addEventListener(type, handler);
        record.cleanup.push(() => element.removeEventListener(type, handler));
      };
      function dismiss() {
        if (record.disposed) return;
        record.disposed = true;
        clearTimeout(record.timer);
        record.cleanup.forEach(cleanup => cleanup());
        const restoreFocus = node.contains(document.activeElement);
        node.remove();
        toastRecords.delete(record);
        if (restoreFocus && !destroyed) safeFocus(record.previousFocus);
      }
      record.dismiss = dismiss;
      function schedule() {
        clearTimeout(record.timer);
        record.timer = null;
        if (record.disposed || record.persistent || record.pauses.size || record.remaining <= 0) return;
        record.started = performance.now();
        record.timer = setTimeout(dismiss, record.remaining);
      }
      function pause(reason) {
        if (record.timer !== null) {
          record.remaining = Math.max(0, record.remaining - (performance.now() - record.started));
          clearTimeout(record.timer);
          record.timer = null;
        }
        record.pauses.add(reason);
      }
      function resume(reason) {
        record.pauses.delete(reason);
        if (record.disposed || record.persistent || record.pauses.size) return;
        if (record.remaining <= 0 && requestedDuration > 0) dismiss();
        else schedule();
      }
      if (action) {
        const actionButton = document.createElement("button");
        actionButton.className = "button button--ghost";
        actionButton.type = "button";
        actionButton.textContent = String(action.label || "执行操作");
        toastOn(actionButton, "click", async () => {
          if (actionButton.disabled || record.disposed) return;
          actionButton.disabled = true;
          actionButton.setAttribute("aria-busy", "true");
          try {
            await action.onClick();
            if (!destroyed && !record.disposed) dismiss();
          } catch {
            if (destroyed || record.disposed) return;
            node.dataset.tone = "error";
            node.setAttribute("role", "alert");
            text.textContent = `操作失败：${String(message)}。请重试或关闭通知。`;
            actionButton.disabled = false;
            actionButton.removeAttribute("aria-busy");
          }
        });
        actions.append(actionButton);
      }
      toastOn(close, "click", dismiss);
      toastOn(node, "pointerenter", () => pause("pointer"));
      toastOn(node, "pointerleave", () => resume("pointer"));
      toastOn(node, "focusin", () => pause("focus"));
      toastOn(node, "focusout", event => { if (!node.contains(event.relatedTarget)) resume("focus"); });
      actions.append(close);
      node.append(text, actions);
      toastStack.append(node);
      toastRecords.add(record);
      schedule();
      return dismiss;
    }

    on(root, "click", delegate);
    on(root, "keydown", delegate);
    on(document, "pointerdown", event => {
      if (activeMenu && !activeMenu.node.contains(event.target) && !activeMenu.trigger.contains(event.target)) closeMenu(false);
    }, true);
    on(document, "focusin", event => {
      if (activeMenu && !activeMenu.node.contains(event.target) && !activeMenu.trigger.contains(event.target)) closeMenu(false);
    });
    ownedAll("[data-frost-menu]").forEach(trigger => {
      trigger.setAttribute("aria-haspopup", "menu");
      trigger.setAttribute("aria-controls", trigger.dataset.frostMenu);
      trigger.setAttribute("aria-expanded", "false");
    });
    const controller = Object.freeze({
      toast,
      destroy() {
        if (destroyed) return;
        destroyed = true;
        closeMenu(false);
        closeDialog(false);
        Array.from(toastRecords).forEach(record => record.dismiss());
        disposers.splice(0).forEach(dispose => dispose());
        portal?.remove();
        portal = null;
        toastStack = null;
        themeSubscribers.delete(updateThemeUI);
        if (!themeSubscribers.size) listenMedia(false);
        instances.delete(root);
      }
    });
    instances.set(root, controller);
    initFlow();
    themeSubscribers.add(updateThemeUI);
    listenMedia(true);
    renderTheme();
    return controller;
  }

  window.Frost = Object.freeze({ version: "1.0.0", init });
})();
