/** Runtime settings: environment default, saved override, value in force. */

import { h, icon } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api } from "/assets/shared/js/api.js";
import { card, pill, confirm, toast } from "/assets/shared/js/components/ui.js";

const GIB = 2 ** 30;

export default {
  title: "Settings",
  subtitle: "Applied immediately and kept across restarts",

  async render(ctx) {
    const data = await api.get("/admin/api/settings");

    const quota = h("input", {
      class: "input",
      type: "number",
      min: "0",
      step: "0.1",
      value: data.current.maxBytesPerUser ? +(data.current.maxBytesPerUser / GIB).toFixed(2) : 0,
    });
    const backups = h("input", {
      class: "input",
      type: "number",
      min: "1",
      step: "1",
      value: data.current.backupsPerGameLimit,
    });
    const allowed = h("input", {
      class: "input",
      type: "text",
      placeholder: "ids or usernames, comma separated",
      value: data.current.allowedUsers.join(", "),
    });
    const autoDelete = h("input", { type: "checkbox", checked: data.current.autoDeleteSaves });

    const save = async (event) => {
      const button = event.target;
      button.disabled = true;
      try {
        await api.put("/admin/api/settings", {
          maxBytesPerUser: Math.max(0, Math.round(Number(quota.value || 0) * GIB)),
          backupsPerGameLimit: Number(backups.value || 1),
          autoDeleteSaves: autoDelete.checked,
          allowedUsers: allowed.value.split(",").map((entry) => entry.trim()).filter(Boolean),
        });
        toast("Settings saved — in force now", "good");
        ctx.refresh();
      } catch (error) {
        toast(error.message, "critical");
      } finally {
        button.disabled = false;
      }
    };

    const reset = async () => {
      const ok = await confirm({
        title: "Clear panel overrides?",
        body: "Every setting goes back to the value from the server environment.",
        confirmLabel: "Reset",
      });
      if (!ok) return;
      await api.del("/admin/api/settings");
      toast("Overrides cleared", "good");
      ctx.refresh();
    };

    return h(
      "div",
      { class: "grid split" },
      card({
        title: "Limits and access",
        subtitle: data.overrides.length ? "overridden here" : "using environment values",
        body: h(
          "div",
          { class: "card-body", style: { display: "grid", gap: "16px" } },
          field(
            "Per-user storage quota (GiB)",
            quota,
            `0 means unlimited. Environment default: ${fmt.quota(data.defaults.maxBytesPerUser)}.`,
          ),
          field(
            "Legacy backups kept per game",
            backups,
            `Frozen backups are exempt. Environment default: ${data.defaults.backupsPerGameLimit}. Cloud Save V2 keeps one snapshot per game regardless.`,
          ),
          checkField(
            "Delete a save the launcher replaces",
            autoDelete,
            `Off, the previous cloud save version of a game and the previous save in an emulator slot are kept instead of deleted — still on the owner's quota, and theirs to delete from the portal or yours from Saves. Never-finished uploads are swept either way. Environment default: ${data.defaults.autoDeleteSaves ? "on" : "off"}. Per-user exceptions live on each user's page.`,
          ),
          field(
            "Allowed users",
            allowed,
            data.defaults.allowedUsers.length
              ? `Environment default: ${data.defaults.allowedUsers.join(", ")}.`
              : "Empty means everyone with a valid official Hydra login may use this server.",
          ),
          h(
            "div",
            { class: "row", style: { gap: "8px" } },
            h("button", { class: "btn primary", text: "Save changes", onclick: save }),
            h("button", { class: "btn", text: "Reset to environment", onclick: reset }),
          ),
        ),
      }),
      h(
        "div",
        { class: "grid" },
        card({
          title: "In force",
          body: h(
            "div",
            { class: "card-body" },
            h(
              "dl",
              { class: "kv" },
              h("dt", { text: "Quota" }),
              h("dd", {}, fmt.quota(data.current.maxBytesPerUser), source(data, "max_bytes_per_user")),
              h("dt", { text: "Backups per game" }),
              h("dd", {}, String(data.current.backupsPerGameLimit), source(data, "backups_per_game_limit")),
              h("dt", { text: "Delete replaced saves" }),
              h(
                "dd",
                {},
                data.current.autoDeleteSaves ? "yes" : "no — older versions are kept",
                source(data, "auto_delete_saves"),
              ),
              h("dt", { text: "Allowed users" }),
              h(
                "dd",
                {},
                data.current.allowedUsers.length ? data.current.allowedUsers.join(", ") : "everyone",
                source(data, "allowed_users"),
              ),
            ),
          ),
        }),
        card({
          title: "Environment",
          subtitle: "read-only — set before the server starts",
          body: h(
            "div",
            { class: "card-body" },
            h(
              "dl",
              { class: "kv" },
              h("dt", { text: "Public URL" }),
              h("dd", { class: "mono small", text: data.environment.publicUrl }),
              h("dt", { text: "Bind address" }),
              h("dd", { class: "mono small", text: data.environment.bind }),
              h("dt", { text: "Official Hydra API" }),
              h("dd", { class: "mono small", text: data.environment.officialApiUrl }),
              h("dt", { text: "Data directory" }),
              h("dd", { class: "mono small", text: data.environment.dataDir }),
            ),
          ),
        }),
        proxyCard(data.proxy),
      ),
    );
  },
};

/**
 * What the server made of this very request.
 *
 * Behind a proxy every address it records is a header's word, and the failure
 * is silent: sign-ins get logged from the proxy and one visitor's fumbled
 * password locks out everyone. This is the screen that says which address
 * won and why.
 */
function proxyCard(proxy) {
  if (!proxy) return null;

  const rows = [
    ["Your address", h("span", { class: "mono small", text: proxy.clientIp })],
    ["Taken from", h("span", { class: "mono small", text: proxy.source })],
    ["Connection from", h("span", { class: "mono small", text: proxy.peer })],
    [
      "Proxy headers",
      proxy.trustProxyHeaders ? pill("trusted", "good") : pill("ignored", "warning"),
    ],
  ];

  if (proxy.trustProxyHeaders && proxy.header) {
    rows.push(["Configured header", h("span", { class: "mono small", text: proxy.header })]);
  }
  if (proxy.trustProxyHeaders && !proxy.header) {
    rows.push([
      "Forwarded-for hops",
      h("span", { class: "mono small", text: String(proxy.hops) }),
    ]);
  }

  return card({
    title: "Client addresses",
    subtitle: "as seen on this request",
    body: h(
      "div",
      { class: "card-body", style: { display: "grid", gap: "14px" } },
      proxy.ignoringHeaders
        ? h(
            "div",
            { class: "alert warning" },
            icon("warning", 18),
            h(
              "div",
              { class: "stack", style: { flex: 1 } },
              h("div", { class: "title", text: "A proxy is forwarding to this server, but its headers are ignored" }),
              h("div", {
                class: "detail",
                text: "Every sign-in is being recorded from the proxy's address, and one visitor's failed attempts lock out everyone behind it. Set HYDRA_TRUST_PROXY_HEADERS=true — but only once the proxy is the sole way in, since the headers are forgeable by anything that can reach this server directly.",
              }),
            ),
          )
        : null,
      h(
        "dl",
        { class: "kv" },
        ...rows.flatMap(([label, value]) => [h("dt", { text: label }), h("dd", {}, value)]),
      ),
      proxy.observed.length
        ? h(
            "div",
            { class: "stack", style: { gap: "6px" } },
            h("span", { class: "muted small", text: "Headers on this request" }),
            ...proxy.observed.map((header) =>
              h(
                "div",
                { class: "row", style: { gap: "10px", alignItems: "baseline" } },
                h("span", {
                  class: "mono small",
                  style: { color: header.name === proxy.source ? "var(--accent)" : null },
                  text: header.name,
                }),
                h("span", { class: "mono small muted truncate", text: header.value }),
              ),
            ),
          )
        : h("span", {
            class: "muted small",
            text: "No forwarding headers arrived — this request reached the server directly.",
          }),
    ),
  });
}

/** A field whose control is a checkbox, so the label sits beside it. */
function checkField(label, input, hint) {
  return h(
    "div",
    { class: "field" },
    h("label", { class: "checkline" }, input, h("span", { text: label })),
    h("span", { class: "hint", text: hint }),
  );
}

function field(label, input, hint) {
  return h(
    "div",
    { class: "field" },
    h("label", { text: label }),
    input,
    h("span", { class: "hint", text: hint }),
  );
}

/** Marks which layer a value came from, so nothing is mysterious. */
function source(data, key) {
  const override = data.overrides.find((entry) => entry.key === key);
  return h(
    "span",
    { style: { marginLeft: "8px" } },
    override ? pill(`overridden ${fmt.relative(override.updatedAt)}`, "accent") : pill("from environment"),
  );
}
