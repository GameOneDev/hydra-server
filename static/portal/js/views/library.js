/** Achievements, custom images, souvenirs, shares and download sources. */

import { h, icon } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api } from "/assets/shared/js/api.js";
import {
  card,
  gameCell,
  meter,
  pill,
  emptyState,
  confirm,
  toast,
} from "/assets/shared/js/components/ui.js";

export default {
  async render() {
    const library = await api.get("/portal/api/library");

    return h(
      "div",
      { class: "grid" },
      card({
        title: "Achievements",
        subtitle: fmt.plural(library.achievements.length, "game"),
        body: library.achievements.length
          ? table(
              ["Game", "Unlocked", "Synced"],
              library.achievements.map((entry) => [
                gameCell(entry.game, { link: false }),
                h(
                  "div",
                  { class: "row", style: { gap: "8px" } },
                  h("span", { class: "num", text: `${entry.unlocked} / ${entry.total}` }),
                  meter(entry.total ? entry.unlocked / entry.total : 0),
                ),
                h("span", { class: "muted", text: fmt.relative(entry.updatedAt) }),
              ]),
            )
          : emptyState("Nothing synced yet", "Achievements sync while you play.", "trophy"),
      }),
      card({
        title: "Custom images",
        body: library.artwork.length
          ? h(
              "div",
              { class: "card-body row wrap", style: { gap: "12px" } },
              ...library.artwork.map((art) =>
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
                  h("span", { class: "small truncate", text: fmt.gameName(art.game) }),
                  h("span", { class: "muted small", text: art.kind }),
                ),
              ),
            )
          : emptyState("No custom images", "Pick art in the launcher and it syncs here.", "image"),
      }),
      card({
        title: "Souvenirs",
        subtitle: fmt.plural(library.souvenirs.length, "screenshot"),
        body: library.souvenirs.length
          ? h(
              "div",
              { class: "card-body row wrap", style: { gap: "12px" } },
              ...library.souvenirs.map(souvenirTile),
            )
          : emptyState(
              "No souvenirs yet",
              "The launcher takes one when an achievement pops.",
              "trophy",
            ),
      }),
      card({
        title: "Shared backups",
        body:
          library.sharedWithMe.length || library.sharedOut.length
            ? h(
                "div",
                {},
                library.sharedWithMe.length
                  ? h(
                      "div",
                      {},
                      h("div", { class: "card-body tight" }, h("strong", { text: "Shared with you" })),
                      table(
                        ["Game", "From", "Size", "When"],
                        library.sharedWithMe.map((entry) => [
                          gameCell(entry.game, { link: false }),
                          h("span", { text: entry.otherName ?? "another player" }),
                          h("span", { class: "num", text: fmt.bytes(entry.sizeBytes) }),
                          h("span", { class: "muted", text: fmt.relative(entry.createdAt) }),
                        ]),
                      ),
                    )
                  : null,
                library.sharedOut.length
                  ? h(
                      "div",
                      {},
                      h("div", { class: "card-body tight" }, h("strong", { text: "You shared" })),
                      table(
                        ["Game", "With", "Size", "When"],
                        library.sharedOut.map((entry) => [
                          gameCell(entry.game, { link: false }),
                          h("span", { text: entry.otherName ?? "another player" }),
                          h("span", { class: "num", text: fmt.bytes(entry.sizeBytes) }),
                          h("span", { class: "muted", text: fmt.relative(entry.createdAt) }),
                        ]),
                      ),
                    )
                  : null,
              )
            : emptyState("Nothing shared", "Backups you share in the launcher show up here.", "share"),
      }),
      card({
        title: "Download sources",
        subtitle: "synced across your devices",
        body: library.downloadSources.length
          ? table(
              ["Name", "URL", "Added"],
              library.downloadSources.map((entry) => [
                h("span", { text: entry.name || "—" }),
                h("span", { class: "mono truncate", title: entry.url, text: entry.url }),
                h("span", { class: "muted", text: fmt.relative(entry.createdAt) }),
              ]),
            )
          : emptyState("No sources synced", null, "folder"),
      }),
    );
  },
};

/**
 * One souvenir: the picture, what it was taken for, and a way to get rid of it.
 *
 * Deleting here is the same operation the launcher performs — the row and the
 * file both go, and the profile stops showing it.
 */
function souvenirTile(souvenir) {
  return h(
    "div",
    { class: "stack", style: { width: "180px", gap: "4px" } },
    h("img", {
      src: souvenir.url,
      alt: "",
      loading: "lazy",
      style: {
        width: "180px",
        height: "101px",
        objectFit: "cover",
        borderRadius: "6px",
        background: "var(--surface-3)",
      },
    }),
    h("span", { class: "small truncate", text: fmt.gameName(souvenir.game) }),
    h(
      "span",
      { class: "muted small truncate", title: souvenir.primaryAchievementName ?? "" },
      souvenir.primaryAchievementName ?? "—",
      souvenir.achievementCount > 1 ? ` +${souvenir.achievementCount - 1}` : "",
    ),
    h(
      "div",
      { class: "row", style: { gap: "6px", alignItems: "center" } },
      souvenir.visibility === "PRIVATE" ? pill("hidden") : null,
      h("span", { class: "muted small", text: fmt.bytes(souvenir.sizeBytes) }),
      h(
        "button",
        {
          class: "btn small danger",
          title: "Delete",
          "aria-label": "Delete souvenir",
          onclick: async () => {
            const ok = await confirm({
              title: "Delete this souvenir?",
              body: `The screenshot for ${fmt.gameName(souvenir.game)} is removed from this server and disappears from your profile. This cannot be undone.`,
              confirmLabel: "Delete",
              danger: true,
            });
            if (!ok) return;

            const result = await api.del(
              `/portal/api/souvenirs/${encodeURIComponent(souvenir.id)}`,
            );
            toast(`Deleted — ${fmt.bytes(result.freedBytes)} freed`, "good");
            location.reload();
          },
        },
        icon("trash", 14),
      ),
    ),
  );
}

function table(headers, rows) {
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
