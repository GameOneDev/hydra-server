/** The library, pivoted by game instead of by user. */

import { h } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api } from "/assets/shared/js/api.js";
import { setQuery, navigate } from "/assets/shared/js/router.js";
import {
  card,
  statTile,
  gameCell,
  userCell,
  cover,
  pill,
  emptyState,
  toast,
} from "/assets/shared/js/components/ui.js";
import { dataTable, toolbar } from "/assets/shared/js/components/table.js";
import { savesTable } from "/assets/admin/js/views/saves.js";

const games = {
  title: "Games",
  subtitle: "Every game this server holds data for",

  async render(ctx) {
    const { query } = ctx;
    const data = await api.get("/admin/api/games", {
      q: query.q,
      sort: query.sort ?? "storage",
      dir: query.dir ?? "desc",
      page: query.page,
      perPage: 25,
    });

    /* The count of games with no name belongs next to the total: the warning
       pills alone only say how many are on this page. */
    ctx.setHeader({
      title: "Games",
      subtitle: data.unnamed
        ? `${fmt.plural(data.total, "game")} · ${fmt.number(data.unnamed)} without a name`
        : fmt.plural(data.total, "game"),
    });

    return card({
      body: h(
        "div",
        {},
        toolbar({
          search: query.q,
          placeholder: "Search game name or id…",
          onSearch: (value) => setQuery({ q: value, page: null }),
        }),
        dataTable({
          columns: [
            { key: "name", label: "Game", sortable: true, render: (row) => gameCell(row.game) },
            {
              key: "players",
              label: "Players",
              sortable: true,
              align: "right",
              render: (row) => fmt.number(row.players),
            },
            {
              key: "cloudSaves",
              label: "Cloud saves",
              align: "right",
              render: (row) => fmt.number(row.cloudSaves),
            },
            {
              key: "backups",
              label: "Backups",
              align: "right",
              render: (row) => fmt.number(row.backups),
            },
            {
              key: "storage",
              label: "Storage",
              sortable: true,
              align: "right",
              render: (row) => fmt.bytes(row.bytes),
            },
            {
              key: "playtime",
              label: "Playtime",
              sortable: true,
              align: "right",
              render: (row) => fmt.duration(row.playtimeSeconds),
            },
            {
              key: "updated",
              label: "Last activity",
              sortable: true,
              render: (row) => h("span", { class: "muted", text: fmt.relative(row.lastAt) }),
            },
            {
              key: "metadata",
              label: "",
              class: "actions",
              render: (row) =>
                fmt.isGameNamed(row.game) ? null : pill("name unresolved", "warning"),
            },
          ],
          rows: data.rows,
          sort: query.sort ?? "storage",
          dir: query.dir ?? "desc",
          onSort: (sort, dir) => setQuery({ sort, dir, page: null }),
          page: data,
          onPage: (page) => setQuery({ page }),
          onRow: (row) =>
            navigate(
              `/games/${encodeURIComponent(row.game.shop)}/${encodeURIComponent(row.game.objectId)}`,
            ),
          empty: emptyState("No games yet", "Games appear as soon as anything is stored for them.", "games"),
        }),
      ),
    });
  },
};

/** One game: who plays it, what they have stored, what art they picked. */
games.detail = {
  title: "Game",

  async render(ctx) {
    const { shop, objectId } = ctx.params;
    const [detail, saves] = await Promise.all([
      api.get(`/admin/api/games/${encodeURIComponent(shop)}/${encodeURIComponent(objectId)}`),
      api.get("/admin/api/saves", {
        shop,
        objectId,
        sort: ctx.query.sort ?? "updated",
        dir: ctx.query.dir ?? "desc",
        page: ctx.query.page,
        perPage: 15,
      }),
    ]);

    const game = detail.game;
    ctx.setHeader({
      title: fmt.gameName(game),
      subtitle: `${fmt.gameSub(game)} · ${fmt.plural(detail.players.length, "player")}`,
    });

    const totalBytes = detail.players.reduce((sum, player) => sum + player.bytes, 0);

    return h(
      "div",
      { class: "grid" },
      card({
        body: h(
          "div",
          { class: "card-body row wrap", style: { gap: "16px" } },
          cover(game, "lg"),
          h(
            "div",
            { class: "stack", style: { flex: 1, minWidth: 0 } },
            h("strong", { style: { fontSize: "16px" }, text: fmt.gameName(game) }),
            h("span", { class: "muted mono", text: fmt.gameSub(game) }),
            fmt.isGameNamed(game)
              ? null
              : h("span", {}, pill("no name from the store yet", "warning")),
          ),
          h("button", { class: "btn", text: "Back to games", onclick: () => navigate("/games") }),
          h("button", {
            class: "btn",
            text: "Refresh metadata",
            onclick: async (event) => {
              event.target.disabled = true;
              const result = await api.post(
                `/admin/api/games/${encodeURIComponent(shop)}/${encodeURIComponent(objectId)}/refresh`,
              );
              toast(result.resolved ? `Resolved as “${result.name}”` : "The store still doesn't know this id", result.resolved ? "good" : "critical");
              ctx.refresh();
            },
          }),
        ),
      }),
      h(
        "div",
        { class: "grid cols-4" },
        statTile({ label: "Players", value: fmt.number(detail.players.length) }),
        statTile({ label: "Stored", value: fmt.bytes(totalBytes) }),
        statTile({
          label: "Playtime",
          value: fmt.duration(detail.playtime.seconds),
          sub: detail.playtime.days ? `over ${fmt.plural(detail.playtime.days, "day")}` : "never played",
        }),
        statTile({
          label: "Custom images",
          value: fmt.number(detail.artwork.length),
          sub: detail.playtime.lastDay ? `last played ${fmt.date(detail.playtime.lastDay)}` : "",
        }),
      ),
      card({
        title: "Players",
        body: detail.players.length
          ? dataTable({
              columns: [
                { key: "user", label: "User", render: (row) => userCell(row.user) },
                {
                  key: "storage",
                  label: "Storage",
                  align: "right",
                  render: (row) => fmt.bytes(row.bytes),
                },
                {
                  key: "cloud",
                  label: "Cloud saves",
                  align: "right",
                  render: (row) => fmt.number(row.cloudSaves),
                },
                {
                  key: "backups",
                  label: "Backups",
                  align: "right",
                  render: (row) => fmt.number(row.backups),
                },
                {
                  key: "playtime",
                  label: "Playtime",
                  align: "right",
                  render: (row) => fmt.duration(row.playtimeSeconds),
                },
                {
                  key: "last",
                  label: "Last activity",
                  render: (row) => h("span", { class: "muted", text: fmt.relative(row.lastAt) }),
                },
              ],
              rows: detail.players,
              onRow: (row) => navigate(`/users/${encodeURIComponent(row.user.id)}`),
            })
          : emptyState("Nobody has data for this game", null, "users"),
      }),
      card({
        title: "Saves",
        subtitle: fmt.plural(saves.total, "item"),
        body: savesTable({ data: saves, ctx, showUser: true }),
      }),
      detail.artwork.length
        ? card({
            title: "Custom images",
            body: h(
              "div",
              { class: "card-body row wrap", style: { gap: "12px" } },
              ...detail.artwork.map((art) =>
                h(
                  "div",
                  { class: "stack", style: { width: "150px" } },
                  h("img", {
                    src: art.url,
                    alt: "",
                    loading: "lazy",
                    style: {
                      width: "150px",
                      height: "70px",
                      objectFit: "cover",
                      borderRadius: "6px",
                      background: "var(--surface-3)",
                    },
                  }),
                  h("span", { class: "small", text: art.kind }),
                  h("span", { class: "muted small", text: art.source === "upload" ? fmt.bytes(art.sizeBytes) : "SteamGridDB" }),
                ),
              ),
            ),
          })
        : null,
    );
  },
};

export default games;
