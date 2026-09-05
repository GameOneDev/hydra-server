/** The dashboard: is anything wrong, what is here, what changed lately. */

import { h, icon, fill } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api } from "/assets/shared/js/api.js";
import { overview as loadOverview } from "/assets/admin/js/store.js";
import { navigate } from "/assets/shared/js/router.js";
import { card, statTile, alertRow, emptyState, pill } from "/assets/shared/js/components/ui.js";
import { areaChart, stackedBar, barList, heatmap } from "/assets/shared/js/components/charts.js";

/* Events already carry a human summary, so the feed only has to pick an icon
   and get out of the way. */
const CATEGORY_ICONS = { sync: "saves", admin: "tools", auth: "user", system: "storage" };
const SEVERITY_TONE = { critical: "critical", warning: "warning", info: "" };

export default {
  title: "Overview",
  subtitle: "Everything this server is holding right now",

  async render(ctx) {
    const [data, trends, activity, playtime] = await Promise.all([
      loadOverview({ force: true }),
      api.get("/admin/api/trends", { days: 30 }),
      api.get("/admin/api/events", { perPage: 12 }),
      api.get("/admin/api/playtime", { days: 364 }),
    ]);

    ctx.setHeader({
      title: "Overview",
      subtitle: `${fmt.plural(data.users.total, "user")} · ${fmt.bytes(data.server.storedBytes)} stored · up ${fmt.duration(data.server.uptimeSeconds)}`,
    });

    return h(
      "div",
      { class: "grid" },
      data.alerts.length ? alerts(data.alerts) : null,
      tiles(data),
      h(
        "div",
        { class: "grid split" },
        activityChart(trends),
        card({
          title: "Storage",
          subtitle: fmt.bytes(data.server.storedBytes),
          actions: h("button", {
            class: "btn small",
            text: "Details",
            onclick: () => navigate("/storage"),
          }),
          body: h(
            "div",
            { class: "card-body" },
            stackedBar(data.storage.map((entry) => ({ label: entry.label, value: entry.bytes }))),
            h(
              "dl",
              { class: "kv", style: { marginTop: "16px" } },
              h("dt", { text: "Deduplicated" }),
              h("dd", {
                text: `${fmt.bytes(data.cloudSaves.bytes)} on disk for ${fmt.bytes(data.cloudSaves.logicalBytes)} of files`,
              }),
              h("dt", { text: "Database" }),
              h("dd", { text: fmt.bytes(data.server.databaseBytes) }),
              h("dt", { text: "Per-user quota" }),
              h("dd", { text: fmt.quota(data.settings.maxBytesPerUser) }),
            ),
          ),
        }),
      ),
      h(
        "div",
        { class: "grid split" },
        card({
          title: "Recent activity",
          actions: h("button", {
            class: "btn small",
            text: "Full history",
            onclick: () => navigate("/events"),
          }),
          body: activity.rows.length
            ? h("div", { class: "card-body", style: { display: "grid", gap: "14px" } }, ...activity.rows.map(activityRow))
            : emptyState("Nothing has happened yet", "Activity shows up here as it arrives.", "clock"),
        }),
        h(
          "div",
          { class: "grid" },
          card({
            title: "Top users by storage",
            body: h(
              "div",
              { class: "card-body" },
              trends.topUsers.length
                ? barList(
                    trends.topUsers.map((entry) => ({
                      label: h("a", {
                        class: "truncate",
                        href: `#/users/${encodeURIComponent(entry.user.id)}`,
                        text: entry.user.displayName || entry.user.id,
                      }),
                      value: entry.bytes,
                    })),
                  )
                : emptyState("No users yet", null, "users"),
            ),
          }),
          launcherVersions(data.users.launchers),
          card({
            title: "Top games by storage",
            body: h(
              "div",
              { class: "card-body" },
              trends.topGames.length
                ? barList(
                    trends.topGames.map((entry) => ({
                      label: h("a", {
                        class: "truncate",
                        href: `#/games/${encodeURIComponent(entry.game.shop)}/${encodeURIComponent(entry.game.objectId)}`,
                        text: fmt.gameName(entry.game),
                      }),
                      value: entry.bytes,
                    })),
                  )
                : emptyState("No games yet", null, "games"),
            ),
          }),
        ),
      ),
      card({
        title: "Playtime",
        subtitle: `${fmt.duration(data.library.playtimeSeconds)} across all users`,
        body: h("div", { class: "card-body" }, heatmap(playtime, { aggregate: true })),
      }),
    );
  },
};

/** What the fleet is running, most-used build first. */
function launcherVersions(launchers = []) {
  const known = launchers.filter((entry) => entry.version);
  const unknown = launchers.find((entry) => !entry.version);

  return card({
    title: "Launcher versions",
    subtitle: known.length ? `${fmt.plural(known.length, "version")} in use` : null,
    body: h(
      "div",
      { class: "card-body" },
      launchers.length
        ? barList(
            launchers.map((entry) => ({
              label: h("span", {
                class: entry.version ? "mono" : "muted",
                text: entry.version ? `v${entry.version}` : "not reported yet",
              }),
              value: entry.users,
            })),
            { formatValue: fmt.number },
          )
        : emptyState("No users yet", null, "users"),
      unknown
        ? h("div", {
            class: "muted small",
            style: { marginTop: "12px" },
            text: `${fmt.plural(unknown.users, "account")} ${
              unknown.users === 1 ? "hasn't" : "haven't"
            } synced from a launcher since this landed.`,
          })
        : null,
    ),
  });
}

function alerts(list) {
  return card({
    title: "Needs attention",
    subtitle: fmt.plural(list.length, "item"),
    body: h("div", { class: "card-body", style: { display: "grid", gap: "10px" } }, ...list.map(alertRow)),
  });
}

function tiles(data) {
  return h(
    "div",
    { class: "grid cols-4" },
    statTile({
      label: "Users",
      value: fmt.number(data.users.total),
      sub: `${fmt.number(data.users.active7d)} active this week · ${fmt.number(data.users.new7d)} new`,
      href: "#/users",
    }),
    statTile({
      label: "Stored",
      value: fmt.bytes(data.server.storedBytes),
      sub: `${fmt.bytes(data.server.databaseBytes)} database`,
      href: "#/storage",
    }),
    statTile({
      label: "Cloud saves (v2)",
      value: fmt.number(data.cloudSaves.committed),
      sub: `${fmt.number(data.cloudSaves.files)} files${data.cloudSaves.pending ? ` · ${data.cloudSaves.pending} pending` : ""}`,
      href: "#/saves?type=cloud",
    }),
    statTile({
      label: "Backups & saves",
      value: fmt.number(data.backups.total + data.emulationSaves.total),
      sub: `${fmt.number(data.backups.total)} legacy · ${fmt.number(data.emulationSaves.total)} emulation`,
      href: "#/saves?type=legacy",
    }),
  );
}

/** Activity over time, switchable between "how often" and "how much". */
function activityChart(trends) {
  const modes = {
    events: {
      label: "Events",
      value: (point) =>
        Object.entries(point)
          .filter(([key]) => key !== "day" && key !== "bytes")
          .reduce((sum, [, count]) => sum + count, 0),
      format: fmt.number,
    },
    bytes: { label: "Uploaded", value: (point) => point.bytes ?? 0, format: fmt.bytes },
  };

  let mode = "events";
  const plot = h("div", { class: "card-body" });

  /* The server only returns days that had something happen. A time series
     with the quiet days missing lies about its own shape, so fill them. */
  const byDay = new Map(trends.series.map((point) => [point.day, point]));
  const window = [];
  const cursor = new Date();
  cursor.setDate(cursor.getDate() - (trends.days - 1));
  for (let index = 0; index < trends.days; index += 1) {
    const day = cursor.toISOString().slice(0, 10);
    window.push(byDay.get(day) ?? { day });
    cursor.setDate(cursor.getDate() + 1);
  }

  const paint = () => {
    const { value, format } = modes[mode];
    const points = window.map((point) => ({ day: point.day, value: value(point) }));

    fill(
      plot,
      points.length
        ? areaChart(points, {
            formatValue: format,
            breakdown: (point) => {
              const source = byDay.get(point.day) ?? {};
              return Object.entries(source)
                .filter(([key]) => CATEGORY_ICONS[key])
                .map(([key, count]) => [key, fmt.number(count)]);
            },
          })
        : emptyState("No activity in this window", "Charts fill in as launchers sync.", "clock"),
    );
  };

  const toggle = h(
    "div",
    { class: "segmented" },
    ...Object.entries(modes).map(([key, config]) =>
      h("button", {
        "aria-pressed": String(key === mode),
        text: config.label,
        onclick: (event) => {
          mode = key;
          for (const button of event.target.parentElement.children) {
            button.setAttribute("aria-pressed", String(button === event.target));
          }
          paint();
        },
      }),
    ),
  );

  paint();
  return card({
    title: "Activity",
    subtitle: `last ${trends.days} days`,
    actions: toggle,
    body: plot,
  });
}

function activityRow(entry) {
  return h(
    "div",
    { class: "row", style: { alignItems: "flex-start", gap: "12px" } },
    icon(CATEGORY_ICONS[entry.category] ?? "dot", 15),
    h(
      "div",
      { class: "stack", style: { flex: 1, minWidth: 0 } },
      h(
        "div",
        { class: "row", style: { gap: "6px" } },
        entry.user?.id
          ? h("a", {
              class: "strong truncate",
              href: `#/users/${encodeURIComponent(entry.user.id)}`,
              text: entry.user.displayName || entry.user.id,
            })
          : h("span", { class: "strong", text: entry.actor ?? "server" }),
        entry.severity !== "info" ? pill(entry.severity, SEVERITY_TONE[entry.severity]) : null,
      ),
      h("span", { class: "small truncate", text: entry.summary }),
      entry.game?.objectId
        ? h("span", { class: "muted small truncate", text: fmt.gameName(entry.game) })
        : null,
    ),
    h(
      "div",
      { class: "stack right" },
      h("span", { class: "muted small", text: fmt.relative(entry.at) }),
      entry.sizeBytes ? h("span", { class: "small num", text: fmt.bytes(entry.sizeBytes) }) : null,
    ),
  );
}
