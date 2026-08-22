/** What this server is holding for you, and where it came from. */

import { h, icon } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api } from "/assets/shared/js/api.js";
import { card, statTile, meter, emptyState } from "/assets/shared/js/components/ui.js";
import { stackedBar, heatmap } from "/assets/shared/js/components/charts.js";

export default {
  async render() {
    const [overview, playtime] = await Promise.all([
      api.get("/portal/api/overview"),
      api.get("/portal/api/playtime"),
    ]);

    return h(
      "div",
      { class: "grid" },
      h(
        "div",
        { class: "grid cols-4" },
        statTile({
          label: "Stored here",
          value: fmt.bytes(overview.usedBytes),
          sub: overview.quotaBytes
            ? `${fmt.percent(overview.quotaRatio)} of your ${fmt.bytes(overview.quotaBytes)}`
            : "no limit set",
        }),
        statTile({
          label: "Cloud saves",
          value: fmt.number(overview.counts.cloudSaves),
          sub: `${fmt.plural(overview.counts.backups, "older backup")}`,
        }),
        statTile({
          label: "Emulation saves",
          value: fmt.number(overview.counts.emulationSaves),
          sub: `${fmt.plural(overview.counts.artwork, "custom image")}`,
        }),
        statTile({
          label: "Playtime",
          value: fmt.duration(overview.playtimeSeconds),
          sub: `${fmt.plural(overview.counts.achievementGames, "game")} with achievements · ${fmt.plural(overview.counts.souvenirs, "souvenir")}`,
        }),
      ),
      h(
        "div",
        { class: "grid split" },
        card({
          title: "Where your space goes",
          body: h(
            "div",
            { class: "card-body" },
            stackedBar(overview.storage.map((entry) => ({ label: entry.label, value: entry.bytes }))),
            overview.quotaBytes
              ? h(
                  "div",
                  { style: { marginTop: "16px" } },
                  meter(overview.quotaRatio),
                  h("div", {
                    class: "muted small",
                    style: { marginTop: "6px" },
                    text: `${fmt.bytes(overview.usedBytes)} of ${fmt.bytes(overview.quotaBytes)} used`,
                  }),
                )
              : null,
          ),
        }),
        card({
          title: "Your devices",
          subtitle: `${overview.devices.length} seen`,
          body: overview.devices.length
            ? h(
                "div",
                { class: "card-body", style: { display: "grid", gap: "12px" } },
                ...overview.devices.map((device) =>
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
                        text: `${device.platform ?? "unknown"} · ${fmt.plural(device.items, "upload")}`,
                      }),
                    ),
                    h("span", { class: "muted small", text: fmt.relative(device.lastSeenAt) }),
                  ),
                ),
              )
            : emptyState("Nothing synced yet", "Sync a game in the launcher and it shows up here.", "device"),
        }),
      ),
      card({
        title: "Recent syncs",
        body: overview.activity.length
          ? h(
              "div",
              { class: "card-body", style: { display: "grid", gap: "12px" } },
              ...overview.activity.map((entry) =>
                h(
                  "div",
                  { class: "row", style: { alignItems: "flex-start", gap: "10px" } },
                  icon("saves", 14),
                  h(
                    "div",
                    { class: "stack", style: { flex: 1, minWidth: 0 } },
                    h("span", { class: "truncate", text: entry.summary }),
                    entry.game?.objectId
                      ? h("span", { class: "muted small truncate", text: fmt.gameName(entry.game) })
                      : null,
                  ),
                  h("span", { class: "muted small", text: fmt.relative(entry.at) }),
                ),
              ),
            )
          : emptyState("No activity yet", null, "clock"),
      }),
      card({
        title: "Your playtime",
        subtitle: fmt.duration(overview.playtimeSeconds),
        body: h("div", { class: "card-body" }, heatmap(playtime)),
      }),
    );
  },
};
