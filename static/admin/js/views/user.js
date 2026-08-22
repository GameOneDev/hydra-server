/** One account: what it stores, where it syncs from, and what can be done to it. */

import { h, icon } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api } from "/assets/shared/js/api.js";
import { setQuery, navigate } from "/assets/shared/js/router.js";
import {
  card,
  statTile,
  avatar,
  pill,
  meter,
  tabs,
  gameCell,
  emptyState,
  confirm,
  openModal,
  toast,
} from "/assets/shared/js/components/ui.js";
import { stackedBar, heatmap, barList } from "/assets/shared/js/components/charts.js";
import { savesTable } from "/assets/admin/js/views/saves.js";

const TABS = [
  { id: "saves", label: "Saves" },
  { id: "achievements", label: "Achievements" },
  { id: "images", label: "Custom images" },
  { id: "sharing", label: "Sharing & sources" },
  { id: "activity", label: "Activity" },
];

export default {
  title: "User",

  async render(ctx) {
    const id = ctx.params.id;
    const tab = ctx.query.tab ?? "saves";

    const [detail, library, playtime] = await Promise.all([
      api.get(`/admin/api/users/${encodeURIComponent(id)}`),
      api.get(`/admin/api/users/${encodeURIComponent(id)}/library`),
      api.get("/admin/api/playtime", { days: 364, userId: id }),
    ]);

    const user = detail.user;
    ctx.setHeader({
      title: user.displayName || user.id,
      subtitle: `${fmt.bytes(user.usedBytes)} stored · last seen ${fmt.relative(user.lastSeenAt)}`,
    });

    const counts = {
      saves: user.counts.cloudSaves + user.counts.backups + user.counts.emulationSaves,
      achievements: library.achievements.length,
      images: library.artwork.length + library.souvenirs.length,
      sharing: library.shares.length + library.downloadSources.length,
      activity: null,
    };

    const panel = h("div", {});
    const renderTab = async () => {
      panel.replaceChildren(await tabContent(tab, { id, ctx, library }));
    };
    renderTab();

    return h(
      "div",
      { class: "grid" },
      identityCard(user, ctx),
      h(
        "div",
        { class: "grid cols-4" },
        statTile({
          label: "Storage used",
          value: fmt.bytes(user.usedBytes),
          sub: user.quotaBytes ? `${fmt.percent(user.quotaRatio)} of ${fmt.bytes(user.quotaBytes)}` : "no quota set",
        }),
        statTile({
          label: "Cloud saves (v2)",
          value: fmt.number(user.counts.cloudSaves),
          sub: `${fmt.plural(user.counts.backups, "legacy backup")}`,
        }),
        statTile({
          label: "Emulation saves",
          value: fmt.number(user.counts.emulationSaves),
          sub: `${fmt.plural(user.counts.artwork, "custom image")}`,
        }),
        statTile({
          label: "Playtime",
          value: fmt.duration(user.playtimeSeconds),
          sub: `${fmt.plural(user.counts.achievementGames, "game")} with achievements · ${fmt.plural(user.counts.souvenirs, "souvenir")}`,
        }),
      ),
      h(
        "div",
        { class: "grid split" },
        card({
          title: "Storage",
          body: h(
            "div",
            { class: "card-body" },
            stackedBar(user.storage.map((entry) => ({ label: entry.label, value: entry.bytes }))),
            user.quotaBytes
              ? h(
                  "div",
                  { style: { marginTop: "16px" } },
                  meter(user.quotaRatio),
                  h("div", {
                    class: "muted small",
                    style: { marginTop: "6px" },
                    text: `${fmt.bytes(user.usedBytes)} of ${fmt.bytes(user.quotaBytes)} quota`,
                  }),
                )
              : null,
          ),
        }),
        card({
          title: "Devices",
          subtitle: `${detail.devices.length} seen`,
          body: detail.devices.length
            ? h(
                "div",
                { class: "card-body", style: { display: "grid", gap: "12px" } },
                ...detail.devices.map((device) =>
                  h(
                    "div",
                    { class: "row" },
                    icon("device", 15),
                    h(
                      "div",
                      { class: "stack", style: { flex: 1, minWidth: 0 } },
                      h("span", { class: "truncate strong", text: device.hostname }),
                      h("span", {
                        class: "muted small",
                        text: `${device.platform ?? "unknown platform"} · ${fmt.plural(device.items, "upload")}`,
                      }),
                    ),
                    h("span", { class: "muted small", text: fmt.relative(device.lastSeenAt) }),
                  ),
                ),
              )
            : emptyState("No uploads yet", "Devices appear once this user syncs something.", "device"),
        }),
      ),
      detail.games.length
        ? card({
            title: "Top games",
            body: h(
              "div",
              { class: "card-body" },
              barList(
                detail.games.slice(0, 8).map((entry) => ({
                  label: h("a", {
                    class: "truncate",
                    href: `#/games/${encodeURIComponent(entry.game.shop)}/${encodeURIComponent(entry.game.objectId)}`,
                    text: fmt.gameName(entry.game),
                  }),
                  value: entry.bytes,
                })),
              ),
            ),
          })
        : null,
      card({
        body: h(
          "div",
          {},
          h(
            "div",
            { class: "card-head" },
            tabs({
              items: TABS.map((item) => ({ ...item, count: counts[item.id] })),
              active: tab,
              onSelect: (next) => setQuery({ tab: next, page: null }),
            }),
          ),
          panel,
        ),
      }),
      card({
        title: "Playtime",
        subtitle: fmt.duration(user.playtimeSeconds),
        body: h("div", { class: "card-body" }, heatmap(playtime)),
      }),
      dangerZone(user, ctx),
    );
  },
};

function identityCard(user, ctx) {
  return card({
    body: h(
      "div",
      { class: "card-body row wrap", style: { gap: "16px" } },
      avatar(user, "lg"),
      h(
        "div",
        { class: "stack", style: { flex: 1, minWidth: 0 } },
        h(
          "div",
          { class: "row wrap", style: { gap: "8px" } },
          h("strong", { style: { fontSize: "16px" }, text: user.displayName || user.id }),
          user.username ? h("span", { class: "muted", text: `@${user.username}` }) : null,
          user.isBlocked ? pill("blocked", "critical") : pill("active", "good"),
        ),
        h("span", { class: "mono muted", text: user.id }),
        h("span", {
          class: "muted small",
          text: `First seen ${fmt.dateTime(user.createdAt)} · last seen ${fmt.dateTime(user.lastSeenAt)}`,
        }),
      ),
      h(
        "div",
        { class: "row", style: { gap: "8px" } },
        h("button", { class: "btn", text: "Back to users", onclick: () => navigate("/users") }),
        h("button", {
          class: "btn",
          text: "Portal link",
          title: "Create a short-lived link that signs this user in to their own portal",
          onclick: async (event) => {
            event.target.disabled = true;
            try {
              const link = await api.post(
                `/admin/api/users/${encodeURIComponent(user.id)}/portal-link`,
              );
              portalLinkDialog(link);
            } catch (error) {
              toast(error.message, "critical");
            } finally {
              event.target.disabled = false;
            }
          },
        }),
        h("button", {
          class: "btn",
          text: user.isBlocked ? "Unblock" : "Block",
          onclick: async () => {
            await api.post(`/admin/api/users/${encodeURIComponent(user.id)}/block`, {
              blocked: !user.isBlocked,
            });
            toast(user.isBlocked ? "User unblocked" : "User blocked", "good");
            ctx.refresh();
          },
        }),
      ),
    ),
  });
}

async function tabContent(tab, { id, ctx, library }) {
  if (tab === "saves") {
    const data = await api.get("/admin/api/saves", {
      userId: id,
      type: ctx.query.type,
      sort: ctx.query.sort ?? "updated",
      dir: ctx.query.dir ?? "desc",
      page: ctx.query.page,
      perPage: 15,
    });
    return savesTable({ data, ctx, showUser: false });
  }

  if (tab === "achievements") {
    if (!library.achievements.length) {
      return emptyState("No achievements synced", null, "trophy");
    }
    return simpleTable(
      ["Game", "Unlocked", "Synced"],
      library.achievements.map((entry) => [
        gameCell(entry.game),
        h(
          "div",
          { class: "row", style: { gap: "8px" } },
          h("span", { class: "num", text: `${entry.unlocked} / ${entry.total}` }),
          meter(entry.total ? entry.unlocked / entry.total : 0),
        ),
        h("span", { class: "muted", text: fmt.relative(entry.updatedAt) }),
      ]),
    );
  }

  if (tab === "images") {
    if (!library.artwork.length && !library.souvenirs.length) {
      return emptyState("No custom images", null, "image");
    }

    const blocks = [];
    if (library.artwork.length) {
      blocks.push(
        h("div", { class: "card-body tight" }, h("strong", { text: "Custom images" })),
        simpleTable(
          ["Game", "Kind", "Source", "Size", "Updated"],
          library.artwork.map((entry) => [
            gameCell(entry.game),
            pill(entry.kind),
            entry.source === "upload" ? pill("uploaded", "accent") : pill("SteamGridDB"),
            h("span", { class: "num", text: entry.sizeBytes ? fmt.bytes(entry.sizeBytes) : "—" }),
            h("span", { class: "muted", text: fmt.relative(entry.updatedAt) }),
          ]),
        ),
      );
    }
    if (library.souvenirs.length) {
      blocks.push(
        h("div", { class: "card-body tight" }, h("strong", { text: "Achievement souvenirs" })),
        simpleTable(
          ["Game", "Achievement", "Visibility", "Likes", "Size", "Captured"],
          library.souvenirs.map((entry) => [
            gameCell(entry.game),
            h(
              "div",
              { class: "row", style: { gap: "8px" } },
              /* The picture itself, because a report is impossible to judge
                 from an achievement name. */
              h("a", {
                class: "mono truncate",
                href: entry.url,
                target: "_blank",
                rel: "noreferrer",
                text: entry.primaryAchievementName || "—",
              }),
              entry.achievementCount > 1 ? pill(`+${entry.achievementCount - 1}`) : null,
              entry.reports ? pill(fmt.plural(entry.reports, "report"), "warning") : null,
            ),
            entry.visibility === "PRIVATE" ? pill("hidden") : pill("public", "accent"),
            h("span", { class: "num", text: fmt.number(entry.likes) }),
            h("span", { class: "num", text: entry.sizeBytes ? fmt.bytes(entry.sizeBytes) : "—" }),
            h("span", { class: "muted", text: fmt.relative(entry.capturedAt) }),
          ]),
        ),
      );
    }

    return h("div", { class: "grid" }, ...blocks);
  }

  if (tab === "sharing") {
    const blocks = [];
    if (library.shares.length) {
      blocks.push(
        h("div", { class: "card-body tight" }, h("strong", { text: "Shared backups" })),
        simpleTable(
          ["Game", "Shared with", "Size", "When"],
          library.shares.map((entry) => [
            gameCell(entry.game),
            h("span", { text: entry.recipientName || entry.recipientUserId }),
            h("span", { class: "num", text: fmt.bytes(entry.sizeBytes) }),
            h("span", { class: "muted", text: fmt.relative(entry.createdAt) }),
          ]),
        ),
      );
    }
    if (library.downloadSources.length) {
      blocks.push(
        h("div", { class: "card-body tight" }, h("strong", { text: "Download sources" })),
        simpleTable(
          ["Name", "URL", "Added"],
          library.downloadSources.map((entry) => [
            h("span", { text: entry.name || "—" }),
            h("span", { class: "mono truncate", title: entry.url, text: entry.url }),
            h("span", { class: "muted", text: fmt.relative(entry.createdAt) }),
          ]),
        ),
      );
    }
    return blocks.length
      ? h("div", {}, ...blocks)
      : emptyState("Nothing shared", "No shared backups or synced download sources.", "share");
  }

  const activity = await api.get("/admin/api/events", { userId: id, perPage: 50 });
  if (!activity.rows.length) return emptyState("No activity recorded", null, "clock");

  return h(
    "div",
    {},
    simpleTable(
      ["What", "Game", "Size", "When"],
      activity.rows.map((entry) => [
        h(
          "div",
          { class: "stack" },
          h("span", { text: entry.summary }),
          h("span", { class: "mono muted small", text: entry.kind }),
        ),
        entry.game?.objectId ? gameCell(entry.game) : h("span", { class: "muted", text: "—" }),
        h("span", { class: "num", text: entry.sizeBytes ? fmt.bytes(entry.sizeBytes) : "" }),
        h("span", { class: "muted", title: fmt.dateTime(entry.at), text: fmt.relative(entry.at) }),
      ]),
    ),
    h(
      "div",
      { class: "card-body tight" },
      h("button", {
        class: "btn small",
        text: "Open in history",
        onclick: () => navigate(`/events?userId=${encodeURIComponent(id)}`),
      }),
    ),
  );
}

/** Small static table for the collections that don't paginate. */
function simpleTable(headers, rows) {
  return h(
    "div",
    { class: "table-wrap" },
    h(
      "table",
      { class: "data" },
      h("thead", {}, h("tr", {}, ...headers.map((label) => h("th", { text: label })))),
      h("tbody", {}, ...rows.map((cells) => h("tr", {}, ...cells.map((cell) => h("td", {}, cell))))),
    ),
  );
}

const PURGE_CATEGORIES = [
  ["cloudSaves", "Cloud saves (v2)"],
  ["backups", "Legacy save backups"],
  ["emulationSaves", "Emulation saves"],
  ["artwork", "Custom images"],
  ["souvenirs", "Achievement souvenirs"],
  ["achievements", "Achievements"],
  ["playtime", "Playtime history"],
  ["downloadSources", "Download sources"],
  ["shares", "Shares they created"],
];

function dangerZone(user, ctx) {
  return card({
    className: "danger-zone",
    title: "Danger zone",
    body: h(
      "div",
      { class: "card-body", style: { display: "grid", gap: "14px" } },
      dangerRow(
        "Delete selected data",
        "Keeps the account and its history — the launcher re-uploads whatever it still has locally.",
        h("button", { class: "btn", text: "Purge data…", onclick: () => purgeDialog(user, ctx) }),
      ),
      dangerRow(
        "Delete this user",
        "Removes the account and every byte it stores here.",
        h("button", {
          class: "btn danger",
          text: "Delete user…",
          onclick: async () => {
            const ok = await confirm({
              title: `Delete ${user.displayName || user.id}?`,
              body: `Everything this account stores here — ${fmt.bytes(user.usedBytes)} across saves, backups and images — is deleted. They can sign in again afterwards and start fresh.`,
              confirmLabel: "Delete everything",
              danger: true,
            });
            if (!ok) return;
            const result = await api.del(`/admin/api/users/${encodeURIComponent(user.id)}`);
            toast(`User deleted — ${fmt.bytes(result.freedBytes)} freed`, "good");
            navigate("/users");
          },
        }),
      ),
    ),
  });
}

/** Shows the minted link with a copy button — it is only useful if it can be
 *  handed to someone before it expires. */
function portalLinkDialog(link) {
  const field = h("input", { class: "input", value: link.url, readonly: true, style: { width: "100%" } });

  openModal({
    title: "Portal sign-in link",
    body: h(
      "div",
      { style: { display: "grid", gap: "10px" } },
      h("p", {
        style: { margin: 0 },
        text: `Send this to the user. It signs them in to their own portal and expires in ${Math.round(link.expiresInSeconds / 60)} minutes.`,
      }),
      field,
    ),
    actions: (close) => [
      h("button", { class: "btn", text: "Close", onclick: () => close() }),
      h("button", {
        class: "btn primary",
        text: "Copy link",
        onclick: async () => {
          try {
            await navigator.clipboard.writeText(link.url);
            toast("Link copied", "good");
          } catch (_) {
            field.select();
            toast("Select and copy the link", "critical");
          }
        },
      }),
    ],
  });
}

/** One labelled action in the danger zone: what it does, then the button. */
function dangerRow(title, detail, button) {
  return h(
    "div",
    { class: "row wrap", style: { gap: "12px" } },
    h(
      "div",
      { class: "stack", style: { flex: 1, minWidth: "240px" } },
      h("strong", { text: title }),
      h("span", { class: "muted small", text: detail }),
    ),
    button,
  );
}

function purgeDialog(user, ctx) {
  const checks = PURGE_CATEGORIES.map(([key, label]) => {
    const input = h("input", { type: "checkbox", value: key });
    return { key, input, node: h("label", { class: "checkline" }, input, h("span", { text: label })) };
  });

  openModal({
    title: `Purge data for ${user.displayName || user.id}`,
    body: h(
      "div",
      {},
      h("p", { style: { marginTop: 0 }, text: "Pick what to delete. The account itself is kept." }),
      ...checks.map((check) => check.node),
    ),
    actions: (close) => [
      h("button", { class: "btn", text: "Cancel", onclick: () => close() }),
      h("button", {
        class: "btn danger",
        text: "Purge",
        onclick: async (event) => {
          const categories = checks.filter((check) => check.input.checked).map((check) => check.key);
          if (!categories.length) {
            toast("Nothing selected", "critical");
            return;
          }
          event.target.disabled = true;
          try {
            const result = await api.post(
              `/admin/api/users/${encodeURIComponent(user.id)}/purge`,
              { categories },
            );
            close();
            toast(`Purged ${result.purged.length} categor${result.purged.length === 1 ? "y" : "ies"} — ${fmt.bytes(result.freedBytes)} freed`, "good");
            ctx.refresh();
          } catch (error) {
            toast(error.message, "critical");
            event.target.disabled = false;
          }
        },
      }),
    ],
  });
}
