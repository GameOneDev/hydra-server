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
