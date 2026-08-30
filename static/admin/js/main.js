/**
 * Boot and wiring.
 *
 * Routes map to view modules; a view exports `title`, optional `subtitle`,
 * and `render(ctx)` returning a node. Adding a screen means adding a file and
 * one line here — nothing else in the panel needs to know about it.
 */

import { api, events } from "/assets/shared/js/api.js";
import { register, current, start, navigate } from "/assets/shared/js/router.js";
import { createShell } from "/assets/admin/js/components/shell.js";
import { openPalette } from "/assets/admin/js/components/palette.js";
import { overview as loadOverview, invalidate } from "/assets/admin/js/store.js";
import { toast, skeleton } from "/assets/shared/js/components/ui.js";
import { h } from "/assets/shared/js/dom.js";
import { hideTip } from "/assets/shared/js/components/charts.js";

import login from "/assets/admin/js/views/login.js";
import overview from "/assets/admin/js/views/overview.js";
import eventsView from "/assets/admin/js/views/events.js";
import users from "/assets/admin/js/views/users.js";
import user from "/assets/admin/js/views/user.js";
import saves from "/assets/admin/js/views/saves.js";
import games from "/assets/admin/js/views/games.js";
import storage from "/assets/admin/js/views/storage.js";
import maintenance from "/assets/admin/js/views/maintenance.js";
import schedule from "/assets/admin/js/views/schedule.js";
import webhooks from "/assets/admin/js/views/webhooks.js";
import settings from "/assets/admin/js/views/settings.js";

register("/", overview);
register("/events", eventsView);
register("/users", users);
register("/users/:id", user);
register("/saves", saves);
register("/games", games);
register("/games/:shop/:objectId", games.detail);
register("/storage", storage);
register("/maintenance", maintenance);
register("/schedule", schedule);
register("/webhooks", webhooks);
register("/settings", settings);

const app = document.getElementById("app");
let shell = null;
let renderToken = 0;
/* The boot probe is *expected* to 401 on a fresh browser; only a session that
   dies mid-use is worth interrupting the operator about. */
let signedIn = false;

async function boot() {
  try {
    await api.get("/admin/api/session");
    showApp();
  } catch (error) {
    if (error.status === 403) {
      /* The panel is switched off entirely — say so instead of looping on a
         login form that can never succeed. */
      app.replaceChildren(
        h("div", { class: "login-page" }, h("div", { class: "login-card" },
          h("h1", { text: "Admin panel disabled" }),
          h("p", { class: "muted small", text: error.message }))),
      );
      return;
    }
    showLogin();
  } finally {
    app.removeAttribute("aria-busy");
  }
}

function showLogin() {
  signedIn = false;
  app.replaceChildren(login.render({ onSuccess: showApp }));
}

function showApp() {
  signedIn = true;
  shell = createShell({ onSearch: openPalette });
  app.replaceChildren(shell.root);
  start(renderRoute);
  refreshChrome();
}

async function refreshChrome() {
  try {
    shell?.setOverview(await loadOverview());
  } catch (_) {
    /* The chrome is decoration; a failed poll must not blank the screen. */
  }
}

async function renderRoute() {
  const route = current();
  hideTip();

  if (!route.view) {
    navigate("/", { replace: true });
    return;
  }

  const token = ++renderToken;
  shell.setActive(route.path);
  shell.setHeader({
    title: typeof route.view.title === "function" ? route.view.title(route) : route.view.title,
    subtitle: typeof route.view.subtitle === "function" ? route.view.subtitle(route) : route.view.subtitle,
  });
  shell.content.replaceChildren(h("section", { class: "card" }, skeleton(5)));

  const ctx = {
    ...route,
    setHeader: (header) => {
      if (token === renderToken) shell.setHeader(header);
    },
    /** Re-runs the whole screen — for actions whose result is the new state. */
    refresh: () => {
      invalidate();
      renderRoute();
      refreshChrome();
    },
    /** Updates the sidebar only, for actions that print their own result and
     *  would lose it to a re-render. */
    refreshChrome: () => {
      invalidate();
      refreshChrome();
    },
  };

  try {
    const node = await route.view.render(ctx);
    /* A slow view whose route has since changed must not paint over the
       screen that replaced it. */
    if (token !== renderToken) return;
    shell.content.replaceChildren(node);
  } catch (error) {
    if (token !== renderToken || error.status === 401) return;
    shell.content.replaceChildren(
      h("section", { class: "card" },
        h("div", { class: "empty" },
          h("div", { class: "title", text: "Couldn't load this screen" }),
          h("div", { class: "small muted", text: error.message }),
          h("button", { class: "btn", text: "Try again", onclick: () => renderRoute() }))),
    );
  }
}

events.addEventListener("unauthorized", () => {
  if (!signedIn) return;
  toast("Session expired — sign in again", "critical");
  showLogin();
});

addEventListener("hydra:refresh", () => {
  invalidate();
  renderRoute();
  refreshChrome();
});

addEventListener("keydown", (event) => {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
    event.preventDefault();
    openPalette();
  }
});

boot();
