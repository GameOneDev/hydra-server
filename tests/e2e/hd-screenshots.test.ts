/* End-to-end check of the 1080p screenshot copies against a RUNNING server.
 *
 * Run it from the launcher checkout — see README.md next to this file. The
 * launcher's own `uploadHdCopy` does the upload and its own
 * `collectHdScreenshotRefs` / `applyHdScreenshotCopies` do the display side, so
 * this fails if either stops agreeing with the server about paths, field names
 * or response shapes. Only the network calls are written out here, and they
 * must stay the ones `screenshot-hd-copies.ts` makes.
 */
import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { describe, it } from "node:test";

import {
  applyHdScreenshotCopies,
  collectHdScreenshotRefs,
  HD_SCREENSHOTS_PATH,
  SCREENSHOT_HD_FEATURE,
} from "./screenshot-hd-copies-core.js";
import {
  uploadHdCopy,
  type HdCopyUploadDeps,
} from "./screenshot-hd-copy-uploader.js";

const BASE = process.env.HYDRA_E2E_URL ?? "http://127.0.0.1:8799";

/* Reading the history needs the admin panel: start the server with
   HYDRA_ADMIN_PASSWORD and pass the same value here. Without it the history
   checks are skipped and the rest still runs. */
const ADMIN_PASSWORD = process.env.HYDRA_E2E_ADMIN_PASSWORD;

/* The stub official API makes the bearer token the user id. */
const call = (
  token: string,
  method: string,
  path: string,
  body?: unknown
): Promise<Response> =>
  fetch(`${BASE}${path}`, {
    method,
    headers: {
      authorization: `Bearer ${token}`,
      "content-type": "application/json",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });

/* What the launcher's real deps throw: an error carrying the response status. */
const failure = async (response: Response) =>
  Object.assign(new Error(`${response.status} ${await response.text()}`), {
    response: { status: response.status },
  });

const IMAGE = Buffer.alloc(2048, 7);

const depsFor = (token: string): HdCopyUploadDeps => ({
  readOriginal: async () => Buffer.from("capture"),
  compress: async () => ({ image: IMAGE, width: 1920, height: 1080 }),
  requestUploadUrl: async (screenshotId, imageLength) => {
    const response = await call(
      token,
      "POST",
      `${HD_SCREENSHOTS_PATH}/${encodeURIComponent(screenshotId)}/upload-url`,
      { imageExt: "jpg", imageLength }
    );
    if (!response.ok) throw await failure(response);
    return response.json();
  },
  putImage: async (presignedUrl, image) => {
    const response = await fetch(presignedUrl, {
      method: "PUT",
      headers: { "Content-Type": "image/jpeg" },
      body: new Uint8Array(image),
    });
    if (!response.ok) throw await failure(response);
  },
  saveCopy: async (screenshotId, copy) => {
    const response = await call(
      token,
      "PUT",
      `${HD_SCREENSHOTS_PATH}/${encodeURIComponent(screenshotId)}`,
      copy
    );
    if (!response.ok) throw await failure(response);
  },
});

const lookup = async (
  token: string,
  screenshots: Array<{ userId: string; id: string }>
) => {
  const response = await call(
    token,
    "POST",
    `${HD_SCREENSHOTS_PATH}/lookup`,
    { screenshots }
  );
  assert.equal(response.status, 200);
  return (await response.json()) as {
    items: Record<string, { url: string; width: number; height: number }>;
  };
};

const official = (id: string, owner: string) => ({
  id,
  owner: { id: owner },
  imageUrl: `https://official.test/${id}-720.jpg`,
  thumbnailUrl: `https://official.test/${id}-thumb.jpg`,
  width: 1280,
  height: 720,
});

describe("1080p screenshot copies against a running server", () => {
  const alice = `alice-${randomUUID().slice(0, 8)}`;
  const bob = `bob-${randomUUID().slice(0, 8)}`;
  const withCopy = randomUUID();
  const withoutCopy = randomUUID();

  it("advertises the capability the launcher gates on", async () => {
    const response = await fetch(`${BASE}/capabilities`);
    const body = (await response.json()) as { features: string[] };

    assert.ok(body.features.includes(SCREENSHOT_HD_FEATURE));
  });

  it("takes an upload made by the launcher's own uploader", async () => {
    const outcome = await uploadHdCopy(
      { screenshotId: withCopy, path: "/captures/a.png", visibility: "PUBLIC" },
      depsFor(alice)
    );

    assert.deepEqual(outcome, { kind: "uploaded" });
  });

  it("serves exactly the bytes that were uploaded", async () => {
    const { items } = await lookup(alice, [{ userId: alice, id: withCopy }]);
    const copy = items[withCopy];

    assert.ok(copy, "the owner finds the copy");
    assert.equal(copy.width, 1920);
    assert.equal(copy.height, 1080);

    const image = await fetch(copy.url);
    assert.equal(image.status, 200);
    assert.equal(image.headers.get("content-type"), "image/jpeg");
    assert.deepEqual(Buffer.from(await image.arrayBuffer()), IMAGE);
  });

  it("swaps the copy into a listing for another member and leaves the rest", async () => {
    const url = `/users/${alice}/screenshots?take=24`;
    const listing = {
      items: [official(withCopy, alice), official(withoutCopy, alice)],
      total: 2,
    };

    const refs = collectHdScreenshotRefs(url, listing);
    assert.equal(refs.length, 2);

    const { items } = await lookup(bob, refs);
    assert.equal(applyHdScreenshotCopies(listing, items), 1);

    assert.notEqual(listing.items[0].imageUrl, `https://official.test/${withCopy}-720.jpg`);
    assert.equal(listing.items[0].width, 1920);
    assert.equal(listing.items[0].height, 1080);
    assert.equal(
      listing.items[1].imageUrl,
      `https://official.test/${withoutCopy}-720.jpg`,
      "a screenshot with no copy keeps official's image"
    );
  });

  it("will not hand one member another member's copy of the same id", async () => {
    const { items } = await lookup(alice, [{ userId: bob, id: withCopy }]);

    assert.deepEqual(items, {}, "bob never attached a copy to that id");
  });

  it("stops handing a private copy to anyone but its owner", async () => {
    const edit = await call(
      alice,
      "PATCH",
      `${HD_SCREENSHOTS_PATH}/${withCopy}`,
      { visibility: "PRIVATE" }
    );
    assert.equal(edit.status, 200);

    assert.deepEqual(
      (await lookup(bob, [{ userId: alice, id: withCopy }])).items,
      {}
    );
    assert.ok(
      (await lookup(alice, [{ userId: alice, id: withCopy }])).items[withCopy]
    );
  });

  it("answers the way the launcher expects for a screenshot with no copy", async () => {
    const edit = await call(
      alice,
      "PATCH",
      `${HD_SCREENSHOTS_PATH}/${withoutCopy}`,
      { visibility: "PUBLIC" }
    );
    assert.equal(edit.status, 404, "an ordinary case the launcher ignores");

    const remove = await call(
      alice,
      "DELETE",
      `${HD_SCREENSHOTS_PATH}/${withoutCopy}`
    );
    assert.equal(remove.status, 200, "deleting nothing is not an error");
  });

  it("refuses an unauthenticated caller", async () => {
    const response = await call("bad", "POST", `${HD_SCREENSHOTS_PATH}/lookup`, {
      screenshots: [],
    });

    assert.equal(response.status, 401);
  });

  it("removes the copy and its file when the screenshot is deleted", async () => {
    const { items } = await lookup(alice, [{ userId: alice, id: withCopy }]);
    const url = items[withCopy].url;

    const remove = await call(
      alice,
      "DELETE",
      `${HD_SCREENSHOTS_PATH}/${withCopy}`
    );
    assert.equal(remove.status, 200);

    assert.deepEqual(
      (await lookup(alice, [{ userId: alice, id: withCopy }])).items,
      {}
    );
    assert.equal((await fetch(url)).status, 404, "the file is gone too");
  });
});

interface HistoryEntry {
  kind: string;
  severity: string;
  /* The title: one line in the panel's list. */
  summary: string;
  user: { id: string } | null;
  sizeBytes: number | null;
  detail: Record<string, unknown> | null;
}

let adminCookie: string | undefined;

/* The panel's Events screen reads this same endpoint. */
const history = async (userId: string): Promise<HistoryEntry[]> => {
  if (!adminCookie) {
    const login = await fetch(`${BASE}/admin/api/login`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ password: ADMIN_PASSWORD }),
    });
    assert.equal(login.status, 200, "the admin password is the server's");
    adminCookie = (login.headers.get("set-cookie") ?? "").split(";")[0];
  }

  const response = await fetch(
    `${BASE}/admin/api/events?kind=hd_screenshot.&userId=${encodeURIComponent(userId)}&perPage=100`,
    { headers: { cookie: adminCookie } }
  );
  assert.equal(response.status, 200);

  const { rows } = (await response.json()) as { rows: HistoryEntry[] };
  return rows
    .filter((row) => row.user?.id === userId)
    .reverse(); /* the panel lists newest first */
};

describe(
  "what the admin panel's history shows about 1080p copies",
  { skip: ADMIN_PASSWORD ? false : "set HYDRA_E2E_ADMIN_PASSWORD" },
  () => {
    const carol = `carol-${randomUUID().slice(0, 8)}`;
    const worked = randomUUID();
    const neverArrived = randomUUID();
    const unreachable = randomUUID();

    it("records an upload that worked, with its size", async () => {
      const outcome = await uploadHdCopy(
        { screenshotId: worked, path: "/captures/a.png", visibility: "PUBLIC" },
        depsFor(carol)
      );
      assert.deepEqual(outcome, { kind: "uploaded" });

      const entries = await history(carol);
      assert.deepEqual(
        entries.map((entry) => entry.kind),
        ["hd_screenshot.upload_started", "hd_screenshot.uploaded"]
      );

      const uploaded = entries[1];
      assert.equal(uploaded.severity, "info");
      assert.equal(uploaded.sizeBytes, IMAGE.byteLength);
      assert.equal(uploaded.detail?.screenshotId, worked);
      assert.match(uploaded.summary, /1920.1080/);
    });

    it("says so when the image never reached storage, and where it was to go", async () => {
      /* The launcher got its slot and then never sent a byte — what a launcher
         that cannot connect to the address does. */
      const deps: HdCopyUploadDeps = {
        ...depsFor(carol),
        putImage: async () => {},
      };

      const outcome = await uploadHdCopy(
        { screenshotId: neverArrived, path: "/c.png", visibility: "PUBLIC" },
        deps
      );

      assert.equal(outcome.kind, "retry", "a 409 is worth another attempt");
      assert.equal(outcome.kind === "retry" && outcome.failure.stage, "save");
      assert.equal(outcome.kind === "retry" && outcome.failure.status, 409);

      const failed = (await history(carol)).filter(
        (entry) =>
          entry.kind === "hd_screenshot.upload_failed" &&
          entry.detail?.screenshotId === neverArrived
      );
      assert.equal(failed.length, 1);
      assert.equal(failed[0].severity, "warning");
      assert.match(failed[0].summary, /never reached storage/);
      assert.ok(failed[0].summary.length < 80, "a title, not a paragraph");

      /* The longer text sits beside the detail, where the panel shows it. */
      const description = String(failed[0].detail?.description);
      assert.ok(description.includes(BASE), `names ${BASE}: ${description}`);
      assert.match(description, /localhost means the launcher itself/);
      assert.equal(failed[0].detail?.reportedBy, "server");
    });

    it("takes the launcher's own account of an upload it could not even attempt", async () => {
      /* The server saw the slot given out and then nothing: no PUT, no save.
         What the launcher knows is the only thing that explains it, so it says
         so with the same request screenshot-hd-copies.ts makes. */
      const connectError = Object.assign(
        new Error("connect ECONNREFUSED 10.0.0.9:8788"),
        { code: "ECONNREFUSED" }
      );
      const deps: HdCopyUploadDeps = {
        ...depsFor(carol),
        putImage: async () => {
          throw connectError;
        },
      };

      const outcome = await uploadHdCopy(
        { screenshotId: unreachable, path: "/d.png", visibility: "PUBLIC" },
        deps
      );
      assert.equal(outcome.kind, "retry");
      if (outcome.kind !== "retry") return;

      assert.equal(outcome.failure.stage, "upload");
      assert.equal(outcome.failure.code, "ECONNREFUSED");
      assert.equal(outcome.failure.target, BASE);

      const report = await call(
        carol,
        "POST",
        `${HD_SCREENSHOTS_PATH}/${unreachable}/failure`,
        { ...outcome.failure, attempts: 1, willRetry: true }
      );
      assert.equal(report.status, 200);
      assert.deepEqual(await report.json(), { ok: true, recorded: true });

      const entries = (await history(carol)).filter(
        (entry) => entry.detail?.screenshotId === unreachable
      );
      assert.deepEqual(
        entries.map((entry) => entry.kind),
        ["hd_screenshot.upload_started", "hd_screenshot.upload_failed"],
        "a slot, and then the reason nothing came of it"
      );

      const [, failed] = entries;
      assert.equal(failed.severity, "warning");
      assert.equal(
        failed.summary,
        "1080p copy failed (sending the image): connect ECONNREFUSED 10.0.0.9:8788"
      );

      const description = String(failed.detail?.description);
      assert.match(description, /sending the image to the storage address/);
      assert.match(description, /ECONNREFUSED 10\.0\.0\.9:8788/);
      assert.match(description, /It will try again\./);
      assert.equal(failed.detail?.reportedBy, "launcher");
      assert.equal(failed.detail?.target, BASE);
    });

    it("refuses a report that is not about a step of an upload", async () => {
      const response = await call(
        carol,
        "POST",
        `${HD_SCREENSHOTS_PATH}/${unreachable}/failure`,
        { stage: "nonsense", message: "x" }
      );

      assert.equal(response.status, 400);
    });

    it("records a delete", async () => {
      const response = await call(
        carol,
        "DELETE",
        `${HD_SCREENSHOTS_PATH}/${worked}`
      );
      assert.equal(response.status, 200);

      const deleted = (await history(carol)).filter(
        (entry) => entry.kind === "hd_screenshot.deleted"
      );
      assert.equal(deleted.length, 1);
      assert.equal(deleted[0].detail?.screenshotId, worked);
    });
  }
);
