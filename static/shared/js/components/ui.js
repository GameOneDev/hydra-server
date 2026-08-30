/** Shared pieces of chrome: tiles, identities, pills, overlays, toasts. */

import { h, icon, frag, initials } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { navigate } from "/assets/shared/js/router.js";

// ------------------------------------------------------------------ layout

export function card({ title, subtitle, actions, body, className = "" }) {
  const head =
    title || actions
      ? h(
          "div",
          { class: "card-head" },
          title ? h("h2", { text: title }) : null,
          subtitle ? h("span", { class: "muted small", text: subtitle }) : null,
          h("span", { class: "spacer" }),
          actions ?? null,
        )
      : null;

  return h("section", { class: `card ${className}` }, head, body);
}

export function statTile({ label, value, sub, hint, tone, onClick, href }) {
  const tile = h(
    href || onClick ? "a" : "div",
    {
      class: `stat${href || onClick ? " clickable" : ""}`,
      href: href ?? null,
      onclick: onClick ?? null,
    },
    h("div", { class: "label" }, label, hint ? tooltipIcon(hint) : null),
    h("div", { class: "value" }, value),
    sub ? h("div", { class: `sub${tone ? ` ${tone}` : ""}`, text: sub }) : null,
  );
  return tile;
}

function tooltipIcon(text) {
  const node = icon("info", 12);
  node.setAttribute("title", text);
  return node;
}

export function tabs({ items, active, onSelect }) {
  return h(
    "div",
    { class: "tabs", role: "tablist" },
    ...items.map((item) =>
      h(
        "button",
        {
          role: "tab",
          "aria-selected": String(item.id === active),
          onclick: () => onSelect(item.id),
        },
        item.label,
        item.count === undefined || item.count === null
          ? null
          : h("span", { class: "count", text: fmt.number(item.count) }),
      ),
    ),
  );
}

export function emptyState(title, detail, iconName = "search") {
  return h(
    "div",
    { class: "empty" },
    icon(iconName, 22),
    h("div", { class: "title", text: title }),
    detail ? h("div", { class: "small", text: detail }) : null,
  );
}

export function skeleton(rows = 4) {
  return h(
    "div",
    { class: "card-body", style: { display: "grid", gap: "12px" } },
    ...Array.from({ length: rows }, (_, index) =>
      h("div", { class: "skeleton", style: { width: `${100 - index * 8}%` } }),
    ),
  );
}

// -------------------------------------------------------------- indicators

export function pill(text, kind = "") {
  return h("span", { class: `pill ${kind}` }, kind ? h("span", { class: "dot" }) : null, text);
}

/** Quota bar. Colour is a threshold, and the number is always beside it. */
export function meter(ratio) {
  const clamped = Math.max(0, Math.min(1, Number(ratio) || 0));
  const tone = clamped >= 1 ? "critical" : clamped >= 0.85 ? "warning" : "";
  return h(
    "div",
    { class: `meter ${tone}`, title: fmt.percent(clamped, 1) },
    h("span", { style: { width: `${clamped * 100}%` } }),
  );
}

export function stateLabel(state) {
  if (state === "pending") return pill("pending", "warning");
  if (state === "committed" || state === "uploaded") return pill("stored", "good");
  /* A version the server kept instead of deleting, because automatic save
     deletion is off. The launcher never sees it; it is here to download or
     delete. */
  if (state === "superseded") return pill("older version", "accent");
  return pill(state ?? "unknown");
}

const KIND_LABELS = {
  cloud: ["Cloud save", "accent"],
  legacy: ["Backup", ""],
  emulation: ["Emulation", ""],
};

export function kindPill(kind) {
  const [label, tone] = KIND_LABELS[kind] ?? [kind, ""];
  return pill(label, tone);
}

// -------------------------------------------------------------- identities

export function avatar(user, size = "") {
  const url = user?.profileImageUrl;
  if (url) {
    const image = h("img", { class: `avatar ${size}`, src: url, alt: "", loading: "lazy" });
    image.addEventListener("error", () => image.replaceWith(fallbackAvatar(user, size)));
    return image;
  }
  return fallbackAvatar(user, size);
}

function fallbackAvatar(user, size) {
  return h("div", { class: `avatar ${size}`, text: initials(user?.displayName || user?.id || "") });
}

export function userCell(user, { link = true } = {}) {
  if (!user?.id) return h("span", { class: "muted", text: "—" });

  return h(
    "div",
    { class: "identity" },
    avatar(user),
    h(
      "div",
      { class: "stack", style: { minWidth: 0 } },
      link
        ? h("a", {
            class: "name truncate",
            href: `#/users/${encodeURIComponent(user.id)}`,
            text: user.displayName || user.id,
          })
        : h("span", { class: "name truncate", text: user.displayName || user.id }),
      h("span", { class: "sub", text: user.username ? `@${user.username}` : user.id }),
    ),
  );
}

/**
 * Game cell with cover art. Covers come from the Steam CDN by app id, so they
 * resolve even when the store lookup for the name didn't — and when the image
 * fails, the box disappears instead of leaving a grey hole.
 */
export function gameCell(game, { link = true } = {}) {
  if (!game?.objectId) return h("span", { class: "muted", text: "—" });

  const href = `#/games/${encodeURIComponent(game.shop)}/${encodeURIComponent(game.objectId)}`;
  return h(
    "div",
    { class: "identity" },
    cover(game),
    h(
      "div",
      { class: "stack", style: { minWidth: 0 } },
      link
        ? h("a", { class: "name truncate", href, text: fmt.gameName(game) })
        : h("span", { class: "name truncate", text: fmt.gameName(game) }),
      h("span", { class: "sub", text: fmt.gameSub(game) }),
    ),
  );
}

export function cover(game, size = "") {
  if (!game?.coverUrl) return null;
  const image = h("img", { class: `cover ${size}`, src: game.coverUrl, alt: "", loading: "lazy" });
  image.addEventListener("error", () => image.remove());
  return image;
}

// ----------------------------------------------------------------- alerts

const ALERT_ICONS = { critical: "critical", warning: "warning", info: "info" };

export function alertRow(alert) {
  return h(
    "div",
    { class: `alert ${alert.level}` },
    icon(ALERT_ICONS[alert.level] ?? "info", 18),
    h(
      "div",
      { class: "stack", style: { flex: 1 } },
      h("div", { class: "title", text: alert.title }),
      h("div", { class: "detail", text: alert.detail }),
    ),
    alert.action
      ? h("button", {
          class: "btn small",
          text: alert.action.label,
          onclick: () => navigate(alert.action.route.replace(/^#/, "")),
        })
      : null,
  );
}

// --------------------------------------------------------------- overlays

/** Modal with a scrim. `onDismiss` fires for Escape and scrim clicks alike,
 *  so a caller waiting on an answer always gets one. */
export function openModal({ title, body, actions, onDismiss }) {
  const scrim = h("div", { class: "scrim" });
  const modal = h("div", { class: "modal", role: "dialog", "aria-modal": "true" });

  let closed = false;
  const close = ({ dismissed = false } = {}) => {
    if (closed) return;
    closed = true;
    scrim.remove();
    removeEventListener("keydown", onKey);
    if (dismissed) onDismiss?.();
  };

  const onKey = (event) => {
    if (event.key === "Escape") close({ dismissed: true });
  };

  modal.append(
    h("h2", { text: title }),
    h("div", { class: "body" }, body),
    h("div", { class: "actions" }, ...actions(close)),
  );

  scrim.append(modal);
  scrim.addEventListener("click", (event) => {
    if (event.target === scrim) close({ dismissed: true });
  });
  addEventListener("keydown", onKey);
  document.body.append(scrim);

  modal.querySelector("button, input, select")?.focus();
  return close;
}

/**
 * Yes/no, resolving false for every way of backing out.
 *
 * `requireText` adds a field that must be typed exactly before the action
 * unlocks — for the handful of operations that can't be undone by clicking
 * again.
 */
export function confirm({ title, body, confirmLabel = "Confirm", danger = false, requireText }) {
  return new Promise((resolve) => {
    const go = h("button", {
      class: `btn ${danger ? "danger" : "primary"}`,
      text: confirmLabel,
      disabled: Boolean(requireText),
    });

    const typed = requireText
      ? h("input", {
          class: "input",
          placeholder: requireText,
          "aria-label": `Type ${requireText} to confirm`,
          oninput: (event) => {
            go.disabled = event.target.value.trim() !== requireText;
          },
          onkeydown: (event) => {
            /* The field has the focus when the modal opens, so Enter should
               mean what the button means. */
            if (event.key === "Enter" && !go.disabled) go.click();
          },
        })
      : null;

    openModal({
      title,
      body: h(
        "div",
        { style: { display: "grid", gap: "10px" } },
        typeof body === "string" ? h("p", { style: { margin: 0 }, text: body }) : body,
        requireText
          ? h(
              "div",
              { class: "field" },
              h("label", { text: `Type “${requireText}” to confirm` }),
              typed,
            )
          : null,
      ),
      onDismiss: () => resolve(false),
      actions: (close) => {
        go.addEventListener("click", () => {
          close();
          resolve(true);
        });
        return [
          h("button", {
            class: "btn",
            text: "Cancel",
            onclick: () => {
              close();
              resolve(false);
            },
          }),
          go,
        ];
      },
    });
  });
}

export function openDrawer({ title, subtitle, body }) {
  const scrim = h("div", { class: "scrim" });
  const close = () => {
    scrim.remove();
    removeEventListener("keydown", onKey);
  };
  const onKey = (event) => {
    if (event.key === "Escape") close();
  };

  const drawer = h(
    "aside",
    { class: "drawer", role: "dialog", "aria-modal": "true" },
    h(
      "div",
      { class: "drawer-head" },
      h(
        "div",
        { class: "stack", style: { minWidth: 0 } },
        h("strong", { class: "truncate", text: title }),
        subtitle ? h("span", { class: "muted small truncate", text: subtitle }) : null,
      ),
      h("span", { class: "spacer" }),
      h("button", { class: "btn ghost icon-only", "aria-label": "Close", onclick: close }, icon("close")),
    ),
    h("div", { class: "drawer-body" }, body),
  );

  scrim.append(drawer);
  scrim.addEventListener("click", (event) => {
    if (event.target === scrim) close();
  });
  addEventListener("keydown", onKey);
  document.body.append(scrim);
  return close;
}

// ----------------------------------------------------------------- toasts

let toastHost = null;

export function toast(message, kind = "") {
  if (!toastHost) {
    toastHost = h("div", { class: "toasts" });
    document.body.append(toastHost);
  }

  const node = h(
    "div",
    { class: `toast ${kind}` },
    icon(kind === "critical" ? "critical" : kind === "good" ? "good" : "info", 16),
    h("div", { text: message }),
  );

  toastHost.append(node);
  setTimeout(() => node.remove(), kind === "critical" ? 7000 : 4000);
  return node;
}

/** Runs an action with a busy button, reporting the outcome once. */
export async function withBusy(button, action, { done } = {}) {
  const label = button.textContent;
  button.disabled = true;
  button.textContent = "Working…";
  try {
    const result = await action();
    if (done) toast(typeof done === "function" ? done(result) : done, "good");
    return result;
  } catch (error) {
    toast(error.message, "critical");
    throw error;
  } finally {
    button.disabled = false;
    button.textContent = label;
  }
}

export { frag };
