/** The frame every screen renders inside: sidebar, top bar, content area. */

import { h, icon, fill } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { navigate } from "/assets/shared/js/router.js";
import { theme, toggleTheme } from "/assets/admin/js/store.js";
import { api } from "/assets/shared/js/api.js";
import { toast } from "/assets/shared/js/components/ui.js";

/** Adding a screen: one entry here, one route in main.js, one view file. */
export const NAV = [
  { group: "Monitor", items: [
    { route: "/", label: "Overview", icon: "overview" },
    { route: "/users", label: "Users", icon: "users", count: (data) => data?.users?.total },
    { route: "/saves", label: "Saves", icon: "saves", count: (data) =>
        (data?.cloudSaves?.committed ?? 0) + (data?.backups?.total ?? 0) + (data?.emulationSaves?.total ?? 0) },
    { route: "/games", label: "Games", icon: "games", count: (data) => data?.library?.games },
    { route: "/events", label: "History", icon: "clock" },
  ] },
  { group: "Operate", items: [
    { route: "/storage", label: "Storage", icon: "storage" },
    { route: "/maintenance", label: "Maintenance", icon: "tools" },
    { route: "/schedule", label: "Schedule", icon: "calendar" },
    { route: "/webhooks", label: "Webhooks", icon: "share" },
    { route: "/settings", label: "Settings", icon: "settings" },
  ] },
];

export function createShell({ onSearch }) {
  const navItems = new Map();

  const nav = h("nav", { class: "nav" });
  for (const group of NAV) {
    nav.append(h("div", { class: "nav-group-label", text: group.group }));
    for (const item of group.items) {
      const count = h("span", { class: "count" });
      const link = h(
        "a",
        { class: "nav-item", href: `#${item.route}` },
        icon(item.icon),
        h("span", { text: item.label }),
        count,
      );
      navItems.set(item.route, { link, count, item });
      nav.append(link);
    }
  }

  const footer = h("div", { class: "sidebar-footer" });

  const sidebar = h(
    "aside",
    { class: "sidebar" },
    h(
      "a",
      { class: "brand", href: "#/" },
      h("div", { class: "brand-mark", text: "H" }),
      h(
        "div",
        { class: "brand-text stack" },
        h("span", { class: "brand-name", text: "Hydra Server" }),
        h("span", { class: "brand-sub", text: "admin console" }),
      ),
    ),
    nav,
    footer,
  );

  const title = h("h1", { text: "Overview" });
  const subtitle = h("span", { class: "subtitle" });
  const actions = h("div", { class: "row", style: { gap: "8px" } });

  const themeButton = h(
    "button",
    { class: "btn ghost icon-only", "aria-label": "Toggle theme", onclick: () => {
      toggleTheme();
      themeButton.replaceChildren(icon(theme() === "light" ? "moon" : "sun"));
      dispatchEvent(new CustomEvent("hydra:theme"));
    } },
    icon(theme() === "light" ? "moon" : "sun"),
  );

  const topbar = h(
    "header",
    { class: "topbar" },
    h("div", { class: "stack" }, title, subtitle),
    h("span", { class: "spacer" }),
    actions,
    h(
      "button",
      { class: "btn", onclick: () => onSearch() },
      icon("search", 14),
      h("span", { text: "Search" }),
      h("span", { class: "kbd", text: "⌘K" }),
    ),
    h("button", { class: "btn ghost icon-only", "aria-label": "Reload", onclick: () =>
      dispatchEvent(new CustomEvent("hydra:refresh")) }, icon("refresh")),
    themeButton,
    h(
      "button",
      {
        class: "btn ghost icon-only",
        "aria-label": "Sign out",
        onclick: async () => {
          await api.post("/admin/api/logout").catch(() => {});
          location.reload();
        },
      },
      icon("logout"),
    ),
  );

  const content = h("main", { class: "content" });
  const root = h("div", { class: "shell" }, sidebar, h("div", { class: "main" }, topbar, content));

  return {
    root,
    content,

    setHeader({ title: pageTitle, subtitle: pageSubtitle, actions: pageActions }) {
      title.textContent = pageTitle ?? "";
      subtitle.textContent = pageSubtitle ?? "";
      fill(actions, pageActions ?? null);
    },

    setActive(path) {
      for (const [route, { link }] of navItems) {
        const active = route === "/" ? path === "/" : path.startsWith(route);
        if (active) link.setAttribute("aria-current", "page");
        else link.removeAttribute("aria-current");
      }
    },

    setOverview(data) {
      for (const { count, item } of navItems.values()) {
        const value = item.count?.(data);
        count.textContent = value === undefined || value === null ? "" : fmt.compact(value);
      }

      fill(
        footer,
        h("span", { text: `v${data?.server?.version ?? "?"}` }),
        h("span", { text: `up ${fmt.duration(data?.server?.uptimeSeconds ?? 0)}` }),
        h("span", { class: "truncate", title: data?.server?.publicUrl ?? "", text: data?.server?.publicUrl ?? "" }),
      );
    },
  };
}

/** Small helper for views: a header action button. */
export function headerButton(label, iconName, onClick, { primary = false } = {}) {
  return h(
    "button",
    { class: `btn${primary ? " primary" : ""}`, onclick: onClick },
    iconName ? icon(iconName, 14) : null,
    h("span", { text: label }),
  );
}

export { toast, navigate };
