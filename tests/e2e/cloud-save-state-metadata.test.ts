/**
 * End-to-end check that an emulator save state's tag survives a round trip:
 * uploaded with `stateMetadata` (launcher 4.1.6+), committed, and read back
 * through the restore manifest and the download URLs — each response checked
 * with the launcher's OWN validators, which reject any key they don't expect.
 *
 * Expects the server on 127.0.0.1:8799 with the stub official API on 9911,
 * like cloud-save-v2.test.ts. Uses its own game, so it can share the server.
 */
import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";

import {
  validateRestoreDownloadUrls,
  validateRestoreManifest,
} from "./cloud-save-contract.js";
import { validatePrepareResponse } from "./upload-local-game-snapshot-helpers.js";

const BASE = "http://127.0.0.1:8799";
const USER = "user-state-metadata";

const sha256 = (text: string) =>
  createHash("sha256").update(text).digest("hex");

const api = async (method: string, path: string, body?: unknown) => {
  const response = await fetch(`${BASE}${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${USER}`,
      ...(body ? { "content-type": "application/json" } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
};

const SAVE = "battery save\n";
const STATE = "save state\n";
const VARIANT_ID = sha256("retroarch-variant");
const TAG = { emulatorId: "retroarch", coreId: "snes9x" };
const GAME = { shop: "launchbox", objectId: "state-metadata-e2e" };

const FILES = [
  {
    variantId: VARIANT_ID,
    rawPath: "<emulator>/retroarch-v2/saves",
    relativePath: "game.srm",
    hash: sha256(SAVE),
    sizeBytes: Buffer.byteLength(SAVE),
    lastModifiedAt: "2026-10-01T10:00:00.000Z",
  },
  {
    variantId: VARIANT_ID,
    rawPath: "<emulator>/retroarch-v2/states",
    relativePath: "game.state1",
    hash: sha256(STATE),
    sizeBytes: Buffer.byteLength(STATE),
    lastModifiedAt: "2026-10-01T10:00:00.000Z",
    stateMetadata: TAG,
  },
];

const bodyByHash: Record<string, string> = {
  [sha256(SAVE)]: SAVE,
  [sha256(STATE)]: STATE,
};

let snapshotId = "";

test("a tagged state uploads and commits", async () => {
  const prepare = await api("POST", "/profile/cloud-saves/prepare-snapshot", {
    ...GAME,
    platform: "linux",
    snapshotHash: sha256("aggregate-with-state"),
    baseVersion: 0,
    retroArchFormatVersion: 2,
    customPathRawPaths: [],
    variants: [{ variantId: VARIANT_ID, kind: "default" }],
    files: FILES,
  });
  assert.equal(prepare.status, 200);

  const prepared = validatePrepareResponse(prepare.body);
  for (const file of prepared.files) {
    if (file.status !== "upload") continue;
    const checksum = file.requiredHeaders["x-amz-checksum-sha256"];
    const content =
      bodyByHash[Buffer.from(checksum, "base64").toString("hex")];
    const upload = await fetch(file.uploadUrl, {
      method: "PUT",
      headers: {
        "Content-Length": String(Buffer.byteLength(content)),
        "x-amz-checksum-sha256": checksum,
      },
      body: content,
    });
    assert.equal(upload.status, 200);
  }

  const commit = await api("POST", "/profile/cloud-saves/commit-snapshot", {
    pendingSnapshotId: prepared.pendingSnapshotId,
  });
  assert.equal(commit.status, 200);
  snapshotId = commit.body.snapshotId;
});

test("the restore manifest returns the tag on the state only", async () => {
  const { status, body } = await api(
    "GET",
    `/profile/cloud-saves/snapshot-restore-manifest?snapshotId=${snapshotId}`
  );
  assert.equal(status, 200);

  const manifest = validateRestoreManifest(body);
  const state = manifest.files.find((f) => f.relativePath === "game.state1");
  const save = manifest.files.find((f) => f.relativePath === "game.srm");

  assert.deepEqual(state?.stateMetadata, TAG);
  assert.equal("stateMetadata" in (save ?? {}), false);
});

test("the download URLs pass the 4.1.6 validator with the tag", async () => {
  const { status, body } = await api(
    "GET",
    `/profile/cloud-saves/snapshot-download-urls?snapshotId=${snapshotId}`
  );
  assert.equal(status, 200);

  // Throws on any unexpected key, including a null stateMetadata.
  const files = validateRestoreDownloadUrls(body);
  const state = files.find((f) => f.relativePath === "game.state1");

  assert.deepEqual(state?.stateMetadata, TAG);
  assert.equal(
    files.filter((f) => f.stateMetadata !== undefined).length,
    1,
    "only the state file carries a tag"
  );
});

test("a malformed tag is refused before anything is stored", async () => {
  const { status } = await api("POST", "/profile/cloud-saves/prepare-snapshot", {
    ...GAME,
    objectId: "state-metadata-e2e-bad",
    platform: "linux",
    snapshotHash: sha256("bad"),
    baseVersion: 0,
    customPathRawPaths: [],
    variants: [{ variantId: VARIANT_ID, kind: "default" }],
    files: [{ ...FILES[1], stateMetadata: { emulatorId: "" } }],
  });
  assert.equal(status, 400);
});
