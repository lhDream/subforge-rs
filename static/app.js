/* 渐进增强脚本：确认弹窗 + 表单弹窗 + 提交中的按钮状态。
 *
 * 这些都不改变服务端行为：没有 JS 时表单照常提交（PRG 模式不变），
 * 删除类操作仍然可完成（只是不弹确认），新增类表单则由服务端渲染成页内表单。
 *
 * 规范依据：§7.1（加载中的提交按钮保留文字与宽度、防止重复提交）、
 * §8（不可逆操作走确认流程并说明对象与后果）、
 * §9（模态弹窗打开后移入焦点、限制焦点在弹窗内、关闭后恢复、提供键盘退出）。
 */
(function () {
  "use strict";

  // ---------------------------------------------------------------- 确认弹窗

  var pending = null;

  function buildDialog() {
    var dialog = document.createElement("dialog");
    dialog.setAttribute("aria-labelledby", "confirm-title");
    dialog.setAttribute("aria-describedby", "confirm-desc");

    var title = document.createElement("h2");
    title.id = "confirm-title";

    var desc = document.createElement("p");
    desc.id = "confirm-desc";

    var actions = document.createElement("div");
    actions.className = "row-actions";

    var cancel = document.createElement("button");
    cancel.type = "button";
    cancel.className = "ghost";
    cancel.textContent = "取消";

    var ok = document.createElement("button");
    ok.type = "button";
    ok.className = "danger";
    // 先给一个默认名称，避免按钮在设置文案之前以无名状态存在于 DOM 中。
    ok.textContent = "确认";

    actions.appendChild(cancel);
    actions.appendChild(ok);
    dialog.appendChild(title);
    dialog.appendChild(desc);
    dialog.appendChild(actions);
    document.body.appendChild(dialog);

    cancel.addEventListener("click", function () {
      dialog.close();
    });

    ok.addEventListener("click", function () {
      var item = pending;
      pending = null;
      dialog.close();
      if (!item) {
        return;
      }
      var form = item.form;
      if (typeof form.requestSubmit === "function") {
        form.requestSubmit(item.submitter);
      } else {
        form.submit();
      }
    });

    return { dialog: dialog, title: title, desc: desc, ok: ok };
  }

  function initConfirm() {
    var triggers = document.querySelectorAll("[data-confirm]");
    if (triggers.length === 0) {
      return;
    }

    var ui = buildDialog();

    triggers.forEach(function (trigger) {
      trigger.addEventListener("click", function (event) {
        var form = trigger.form;
        if (!form) {
          return;
        }
        event.preventDefault();

        pending = { form: form, submitter: trigger };
        ui.title.textContent = trigger.getAttribute("data-confirm-title") || "确认操作";
        ui.desc.textContent = trigger.getAttribute("data-confirm");
        ui.ok.textContent = trigger.getAttribute("data-confirm-ok") || "确认";

        // 默认焦点落在「取消」上，避免误触确认
        ui.dialog.showModal();
        ui.dialog.querySelector("button.ghost").focus();
      });
    });
  }

  // ---------------------------------------------------------------- 表单弹窗

  // 新增类表单装在 <dialog class="modal"> 里，脚本可用时以模态弹窗打开：
  // 焦点限制、Esc 退出、背景 inert 都由浏览器负责。
  //
  // 触发入口是**普通链接**，不是按钮：没有脚本、或浏览器不支持 showModal 时
  // 这里不做任何拦截，链接照常导航到服务端渲染的页内表单
  //（style.css 的 dialog.modal[open]:not([data-modal])），
  // 因此不会出现「点不动的死控件」，也不需要 <noscript> 兜底。
  function initDialogs() {
    document.querySelectorAll("dialog.modal").forEach(function (dialog) {
      if (typeof dialog.showModal !== "function") {
        return;
      }

      var trigger = dialog.id
        ? document.querySelector('[data-dialog-open="' + dialog.id + '"]')
        : null;

      function openDialog() {
        // 服务端可能已经带着 open 渲染（页内表单形态）：先收起来再切成模态，
        // 否则 showModal() 会因为「已经打开但不是模态」抛 InvalidStateError。
        if (dialog.open) {
          dialog.close();
        }
        dialog.setAttribute("data-modal", "");
        dialog.showModal();

        // 校验失败回填时聚焦出错字段，否则聚焦第一个输入框（规范 §9）
        var target =
          dialog.querySelector('[aria-invalid="true"]') ||
          dialog.querySelector("[data-autofocus]");
        if (target) {
          target.focus();
        }
      }

      dialog.querySelectorAll("[data-dialog-close]").forEach(function (button) {
        button.addEventListener("click", function () {
          dialog.close();
        });
      });

      // 点遮罩关闭：::backdrop 上的点击以 dialog 自身为目标。弹窗内边距为 0、
      // 内容铺满，因此内容区的点击不会命中这里。
      dialog.addEventListener("click", function (event) {
        if (event.target === dialog) {
          dialog.close();
        }
      });

      // 关闭后把焦点交还触发入口（规范 §9）。
      // 换成模态时也会触发一次 close 事件，那次是异步派发的，
      // 用 dialog.open 把它排除掉，免得抢走刚移入弹窗的焦点。
      dialog.addEventListener("close", function () {
        if (dialog.open || !trigger) {
          return;
        }
        trigger.focus();
      });

      if (trigger) {
        trigger.addEventListener("click", function (event) {
          event.preventDefault();
          openDialog();
        });
      }

      // 服务端直接渲染成「已打开」= 这次就要展示表单（?new=1 深链，或校验失败回填），
      // 升级成模态弹窗。
      if (dialog.open) {
        openDialog();
      }
    });
  }

  // ---------------------------------------------------------------- 复制链接

  // 「复制链接」按钮默认 hidden：没有 JS 时链接本身仍然可见、可选、可手动复制，
  // 下载链接也是普通 <a>，因此不存在依赖脚本才能完成的操作。
  function initCopy() {
    var buttons = document.querySelectorAll("[data-copy]");
    if (buttons.length === 0) {
      return;
    }

    buttons.forEach(function (button) {
      button.hidden = false;
      button.addEventListener("click", function () {
        var text = button.getAttribute("data-copy") || "";
        // 未配置 BASEURL 时服务端给的是相对路径；复制时补成当前站点地址。
        if (text.charAt(0) === "/") {
          text = window.location.origin + text;
        }
        var original = button.textContent;
        function feedback(ok) {
          button.textContent = ok ? "已复制" : "复制失败";
          button.disabled = true;
          window.setTimeout(function () {
            button.textContent = original;
            button.disabled = false;
          }, 1800);
        }
        copyText(text).then(
          function () {
            feedback(true);
            var warning = button.getAttribute("data-copy-warning");
            if (warning) {
              showStatusToast(warning, "warn");
            } else {
              showStatusToast("订阅链接已复制", "ok");
            }
          },
          function () {
            feedback(false);
            showStatusToast("复制失败，请手动选择链接", "error");
          }
        );
      });
    });
  }

  var statusToastTimer = null;

  // 操作结果提示：优先走 Frost 的规范 toast（AGENTS.md §11.14）——
  // 它自带 role=status/alert、aria-atomic、5 秒自动关闭、悬停与聚焦暂停、
  // 错误常驻，以及手动关闭按钮。只有 Frost 未初始化时才退回本地降级实现。
  // kind: "ok" | "success" | "warn" | "info" | "error"
  function showStatusToast(message, kind) {
    var tone = kind === "ok" || kind === "success" ? "success"
      : kind === "error" ? "error"
      : "info";
    var ui = window.__frostUI;
    if (ui && typeof ui.toast === "function") {
      ui.toast(message, { tone: tone });
      return;
    }
    // 降级：Frost 不可用时仍给出可见反馈，不静默吞掉操作结果。
    var toast = document.getElementById("status-toast");
    if (!toast) {
      toast = document.createElement("div");
      toast.id = "status-toast";
      toast.className = "status-toast";
      toast.setAttribute("role", tone === "error" ? "alert" : "status");
      toast.setAttribute("aria-live", "polite");
      document.body.appendChild(toast);
    }
    toast.textContent = message;
    toast.classList.toggle("warn", kind === "warn");
    toast.classList.toggle("error", tone === "error");
    toast.hidden = false;
    window.clearTimeout(statusToastTimer);
    statusToastTimer = window.setTimeout(function () {
      toast.hidden = true;
    }, 2600);
  }

  function copyText(text) {
    if (navigator.clipboard && window.isSecureContext) {
      return navigator.clipboard.writeText(text).catch(function () {
        return legacyCopy(text);
      });
    }
    return legacyCopy(text);
  }

  function legacyCopy(text) {
    // 非安全上下文（如局域网 HTTP）没有 Clipboard API，退回 execCommand。
    return new Promise(function (resolve, reject) {
      var area = document.createElement("textarea");
      area.value = text;
      area.setAttribute("readonly", "");
      area.style.position = "fixed";
      area.style.top = "-9999px";
      document.body.appendChild(area);
      area.select();
      try {
        var ok = document.execCommand("copy");
        document.body.removeChild(area);
        if (ok) {
          resolve();
        } else {
          reject(new Error("copy failed"));
        }
      } catch (e) {
        document.body.removeChild(area);
        reject(e);
      }
    });
  }

  // ---------------------------------------------------------------- 生成令牌

  // 客户端密码学随机数生成 32 位十六进制令牌。
  // 默认只填入输入框；带 data-generate-save 时连同当前表单一起提交。
  function initTokenGenerator() {
    document.querySelectorAll("[data-generate-token]").forEach(function (button) {
      button.hidden = false;
      button.addEventListener("click", function () {
        var input = document.getElementById(button.getAttribute("data-generate-token"));
        if (!input) {
          return;
        }
        input.value = randomToken();
        input.focus();
        if (button.hasAttribute("data-generate-save")) {
          var form = input.form;
          if (form && typeof form.requestSubmit === "function") {
            form.requestSubmit();
          } else if (form) {
            form.submit();
          }
          return;
        }
        showStatusToast("已生成令牌，点「保存修改」后生效", "warn");
      });
    });
  }

  function randomToken() {
    var bytes = new Uint8Array(16);
    if (window.crypto && window.crypto.getRandomValues) {
      window.crypto.getRandomValues(bytes);
    } else {
      for (var i = 0; i < bytes.length; i++) {
        bytes[i] = Math.floor(Math.random() * 256);
      }
    }
    return Array.from(bytes)
      .map(function (b) {
        return ("0" + b.toString(16)).slice(-2);
      })
      .join("");
  }

  // ---------------------------------------------------------------- 提交状态

  function initBusy() {
    document.addEventListener("submit", function (event) {
      var form = event.target;
      if (!(form instanceof HTMLFormElement) || event.defaultPrevented) {
        return;
      }
      // 延后一拍：先让浏览器取到提交按钮的 name/value，再禁用按钮。
      window.setTimeout(function () {
        form.querySelectorAll("button").forEach(function (button) {
          if (!button.disabled) {
            button.disabled = true;
            button.setAttribute("aria-busy", "true");
          }
        });
      }, 0);
    });
  }

  // ---------------------------------------------------------------- 多选框全选/反选

  // 按钮默认 hidden，由脚本移除 hidden 后才出现：
  // 没有 JS 时按钮不存在，用户仍可逐个勾选，不会留下点不动的死控件。
  function initCheckboxGroups() {
    document.querySelectorAll("[data-checkbox-group]").forEach(function (group) {
      var boxes = group.querySelectorAll('input[type="checkbox"]');
      if (!boxes.length) {
        return;
      }
      var all = group.querySelector("[data-check-all]");
      var invert = group.querySelector("[data-check-invert]");

      function setAll(checked) {
        for (var i = 0; i < boxes.length; i++) {
          boxes[i].checked = checked;
        }
      }

      if (all) {
        all.hidden = false;
        all.addEventListener("click", function () {
          setAll(true);
        });
      }
      if (invert) {
        invert.hidden = false;
        invert.addEventListener("click", function () {
          for (var i = 0; i < boxes.length; i++) {
            boxes[i].checked = !boxes[i].checked;
          }
        });
      }
    });
  }

  // ---------------------------------------------------------------- 分组拖动排序

  // 只增强：手柄默认 hidden；无 JS 时用分组卡片内的上移/下移。
  function initGroupDrag() {
    var form = document.getElementById("group-reorder-form");
    var list = document.querySelector(".entity-list");
    if (!form || !list) {
      return;
    }
    var cards = Array.prototype.slice.call(
      list.querySelectorAll("[data-group-drag]")
    );
    if (cards.length < 2) {
      return;
    }
    var original = cards.map(function (card) {
      return card.getAttribute("data-group-name");
    });
    var dragging = null;

    function currentNames() {
      return Array.prototype.map.call(
        list.querySelectorAll("[data-group-drag]"),
        function (card) {
          return card.getAttribute("data-group-name");
        }
      );
    }

    function submitOrder() {
      var names = currentNames();
      if (names.join("\n") === original.join("\n")) {
        return;
      }
      var oldInputs = form.querySelectorAll('input[name="order"]');
      for (var i = 0; i < oldInputs.length; i++) {
        oldInputs[i].parentNode.removeChild(oldInputs[i]);
      }
      names.forEach(function (name) {
        var input = document.createElement("input");
        input.type = "hidden";
        input.name = "order";
        input.value = name;
        form.appendChild(input);
      });
      form.submit();
    }

    cards.forEach(function (card) {
      var handle = card.querySelector("[data-drag-handle]");
      if (!handle) {
        return;
      }
      handle.hidden = false;
      handle.addEventListener("dragstart", function (event) {
        dragging = card;
        card.classList.add("is-dragging");
        event.dataTransfer.effectAllowed = "move";
        event.dataTransfer.setData(
          "text/plain",
          card.getAttribute("data-group-name") || ""
        );
        if (event.dataTransfer.setDragImage) {
          event.dataTransfer.setDragImage(card, 24, 24);
        }
      });
      handle.addEventListener("dragend", function () {
        card.classList.remove("is-dragging");
        dragging = null;
        submitOrder();
      });
      handle.addEventListener("click", function (event) {
        // 手柄在 <summary> 内，避免点击它时展开/收起分组。
        event.preventDefault();
        event.stopPropagation();
      });
      card.addEventListener("dragover", function (event) {
        if (!dragging || dragging === card) {
          return;
        }
        event.preventDefault();
        var rect = card.getBoundingClientRect();
        var after = event.clientY > rect.top + rect.height / 2;
        if (after) {
          card.parentNode.insertBefore(dragging, card.nextSibling);
        } else {
          card.parentNode.insertBefore(dragging, card);
        }
      });
    });

    list.addEventListener("dragover", function (event) {
      if (dragging) {
        event.preventDefault();
      }
    });
    list.addEventListener("drop", function (event) {
      if (dragging) {
        event.preventDefault();
      }
    });
  }

  // ---------------------------------------------------------------- 手动节点复选框

  // 一个共享的节点选择弹窗：按钮默认 hidden，无 JS 时仍可直接在输入框手填。
  function initNodePicker() {
    var dialog = document.getElementById("node-picker");
    if (!dialog || typeof dialog.showModal !== "function") {
      return;
    }
    var buttons = document.querySelectorAll("[data-node-picker]");
    if (!buttons.length) {
      return;
    }
    var boxes = dialog.querySelectorAll('input[type="checkbox"]');
    var search = dialog.querySelector("#node-picker-search");
    var target = null;
    var unknown = [];
    var known = Object.create(null);

    for (var i = 0; i < boxes.length; i++) {
      known[boxes[i].value] = true;
    }

    function splitNames(value) {
      return value
        .split(/[,，]/)
        .map(function (s) {
          return s.trim();
        })
        .filter(Boolean);
    }

    function filter() {
      var q = search ? search.value.trim().toLowerCase() : "";
      for (var i = 0; i < boxes.length; i++) {
        var label = boxes[i].parentNode;
        var text = (boxes[i].value || "").toLowerCase();
        label.hidden = !!q && text.indexOf(q) < 0;
      }
    }

    buttons.forEach(function (button) {
      button.hidden = false;
      button.addEventListener("click", function () {
        target = document.getElementById(button.getAttribute("data-node-picker"));
        if (!target) {
          return;
        }
        var current = splitNames(target.value);
        unknown = current.filter(function (name) {
          return !known[name];
        });
        for (var i = 0; i < boxes.length; i++) {
          boxes[i].checked = current.indexOf(boxes[i].value) >= 0;
        }
        if (search) {
          search.value = "";
        }
        filter();
        if (!dialog.open) {
          dialog.showModal();
        }
        if (search) {
          search.focus();
        }
      });
    });

    if (search) {
      search.addEventListener("input", filter);
    }

    var all = dialog.querySelector("[data-node-pick-all]");
    if (all) {
      all.addEventListener("click", function () {
        for (var i = 0; i < boxes.length; i++) {
          if (!boxes[i].parentNode.hidden) {
            boxes[i].checked = true;
          }
        }
      });
    }
    var invert = dialog.querySelector("[data-node-pick-invert]");
    if (invert) {
      invert.addEventListener("click", function () {
        for (var i = 0; i < boxes.length; i++) {
          if (!boxes[i].parentNode.hidden) {
            boxes[i].checked = !boxes[i].checked;
          }
        }
      });
    }
    var clear = dialog.querySelector("[data-node-pick-clear]");
    if (clear) {
      clear.addEventListener("click", function () {
        for (var i = 0; i < boxes.length; i++) {
          boxes[i].checked = false;
        }
      });
    }

    var close = dialog.querySelector("[data-node-picker-close]");
    if (close) {
      close.addEventListener("click", function () {
        dialog.close();
      });
    }
    var apply = dialog.querySelector("[data-node-picker-apply]");
    if (apply) {
      apply.addEventListener("click", function () {
        if (!target) {
          return;
        }
        var chosen = [];
        for (var i = 0; i < boxes.length; i++) {
          if (boxes[i].checked) {
            chosen.push(boxes[i].value);
          }
        }
        unknown.forEach(function (name) {
          if (chosen.indexOf(name) < 0) {
            chosen.push(name);
          }
        });
        target.value = chosen.join(", ");
        dialog.close();
      });
    }
  }

  // ---------------------------------------------------------------- 方案分组依赖

  // 勾选分组时自动补全依赖闭包，并给自动加入的项加视觉标记。
  // 服务端保存时仍会再校验一次，不能只依赖脚本。
  function initProfileDeps() {
    var boxes = document.querySelectorAll(
      'input[name="groups"][data-group-deps]'
    );
    if (!boxes.length) {
      return;
    }
    var byName = {};
    boxes.forEach(function (box) {
      byName[box.value] = box;
    });

    function depsOf(box) {
      try {
        var deps = JSON.parse(box.getAttribute("data-group-deps") || "[]");
        return Array.isArray(deps) ? deps : [];
      } catch (e) {
        return [];
      }
    }

    function ensure(box, seen) {
      if (seen[box.value]) {
        return;
      }
      seen[box.value] = true;
      depsOf(box).forEach(function (name) {
        var dep = byName[name];
        if (!dep) {
          return;
        }
        if (!dep.checked) {
          dep.checked = true;
          dep.parentNode.classList.add("auto-added");
        }
        ensure(dep, seen);
      });
    }

    boxes.forEach(function (box) {
      if (box.checked) {
        ensure(box, {});
      }
      box.addEventListener("change", function () {
        if (box.checked) {
          ensure(box, {});
        } else {
          box.parentNode.classList.remove("auto-added");
        }
      });
    });
  }

  // ---------------------------------------------------------------- 返回上一页

  // 错误页的「返回上一页」按钮默认 hidden：没有 JS 时它不会出现，
  // 用户按浏览器返回键同样能拿回已填内容，因此不存在点不动的死控件。
  function initHistoryBack() {
    document.querySelectorAll("[data-history-back]").forEach(function (button) {
      button.hidden = false;
      button.addEventListener("click", function () {
        window.history.back();
      });
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }

  function init() {
    initConfirm();
    initDialogs();
    initCopy();
    initTokenGenerator();
    initBusy();
    initHistoryBack();
    initCheckboxGroups();
    initGroupDrag();
    initNodePicker();
    initProfileDeps();
    initThemeRadio();
  }

  // ---------------------------------------------------------------- 主题单选组
  //
  // 主题是「三选一」的互斥选择，用 radiogroup 语义（而非 Tabs：规范 §11.10
  // 的 role=tab 必须 aria-controls 指向 tabpanel，主题没有面板）。
  // 主题值与偏好存储由 frost.js 统一管理；这里只补 radiogroup 所需的键盘交互
  // 与 aria-checked 同步，不写入第二份主题状态。

  function initThemeRadio() {
    var group = document.querySelector('.theme-switcher[role="radiogroup"]');
    if (!group) {
      return;
    }
    var radios = Array.prototype.slice.call(group.querySelectorAll('[role="radio"]'));
    if (radios.length === 0) {
      return;
    }

    function currentChoice() {
      var checked = radios.filter(function (r) {
        return r.getAttribute("aria-checked") === "true";
      })[0];
      return checked || radios[0];
    }

    // 组内只保留一个 tabindex=0（roving tabindex），其余为 -1。
    function syncTabindex(active) {
      radios.forEach(function (radio) {
        radio.tabIndex = radio === active ? 0 : -1;
      });
    }

    // Frost 在主题变化时更新 aria-pressed，这里据此同步 aria-checked 与 tabindex，
    // 使读屏播报的选中项始终与真实主题一致。
    function syncChecked() {
      radios.forEach(function (radio) {
        var on = radio.getAttribute("aria-pressed") === "true";
        radio.setAttribute("aria-checked", String(on));
      });
      syncTabindex(currentChoice());
    }

    radios.forEach(function (radio) {
      radio.addEventListener("click", function () {
        // Frost 的委托点击会切换主题并更新 aria-pressed；等它执行后再同步。
        window.setTimeout(syncChecked, 0);
      });
      radio.addEventListener("keydown", function (event) {
        if (event.altKey || event.ctrlKey || event.metaKey) {
          return;
        }
        var index = radios.indexOf(radio);
        var destination = null;
        if (event.key === "ArrowRight" || event.key === "ArrowDown") {
          destination = radios[(index + 1) % radios.length];
        } else if (event.key === "ArrowLeft" || event.key === "ArrowUp") {
          destination = radios[(index - 1 + radios.length) % radios.length];
        } else if (event.key === "Home") {
          destination = radios[0];
        } else if (event.key === "End") {
          destination = radios[radios.length - 1];
        } else if (event.key === " " || event.key === "Enter") {
          // Space 可能滚动页面，阻止默认行为后由 click 触发选中。
          event.preventDefault();
          radio.click();
          return;
        }
        if (!destination) {
          return;
        }
        event.preventDefault();
        syncTabindex(destination);
        destination.focus();
      });
    });

    // 初始同步 + 跟随 Frost 后续的主题变化（如系统主题切换）。
    syncChecked();
    var themeRoot = document.querySelector(".appearance-controls[data-frost-root]");
    if (themeRoot) {
      // MutationObserver 只观察 aria-pressed，避免轮询。
      var observer = new MutationObserver(function () {
        syncChecked();
      });
      radios.forEach(function (radio) {
        observer.observe(radio, { attributes: true, attributeFilter: ["aria-pressed"] });
      });
    }
  }

  // 主题 / 毛玻璃由 frost.js 统一管理（首屏由 head 中的 frost.js 同步设置，
  // 交互由 base.html 的 Frost.init 绑定），app.js 不再维护第二套外观逻辑。
})();
