/**
 * End-to-end check of Cloud Save V2 for recompilations (launcher 4.4.0), against
 * a RUNNING server, using the launcher's OWN code at every step it can: its
 * payload builder (which splits a port into the LaunchBox id and a slug), its
 * list query, its delete URL, its strict restore-manifest validator, and the
 * capability it requires before it will sync a port at all.
 *
 * A recompilation is a PC port of a LaunchBox game. Its saves are not the
 * emulator's, so the two must stay apart: a server that ignores the slug files
 * the port's saves under the plain game, and deletes the plain game's saves
 * when the port's are deleted.
 *
 * Run from the launcher checkout — see README.md next to this file.
 */
import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { describe, it } from "node:test";

import { toCloudSaveApiGame } from "../../../shared/recompilations.js";
import { getCloudSaveV2FeaturesForShop } from "./automatic-sync-mode.js";
import {
  validateRemoteSnapshotSummary,
  validateRestoreManifest,
} from "./cloud-save-contract.js";
import { buildDeleteGameCloudSaveSnapshotsUrl } from "./delete-game-cloud-save-data-policy.js";
import { buildPrepareSnapshotPayload } from "./prepare-snapshot-payload.js";
import { validatePrepareResponse } from "./upload-local-game-snapshot-helpers.js";

const BASE = process.env.HYDRA_E2E_URL ?? "http://127.0.0.1:8799";

/* The stub official API makes the bearer token the user id, and a fresh user
   means the data directory need not be empty. */
const USER = `recomp-${randomUUID().slice(0, 8)}`;

const PLAIN = "1234";
const PORT = "1234:soh";

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

const VARIANT_ID = sha256("default-variant");
const VARIANTS = [{ variantId: VARIANT_ID, kind: "default" }];

/** The save's one file, which differs between the emulator and the port. */
const filesFor = (content: string) => [
  {
    variantId: VARIANT_ID,
    rawPath: "<gameDir>",
    relativePath: "save.sav",
    hash: sha256(content),
    sizeBytes: Buffer.byteLength(content),
    lastModifiedAt: "2026-10-01T10:00:00.000Z",
  },
];

/* Built by the launcher's own function, so what is sent is what 4.4.0 sends. */
const payloadFor = (objectId: string, content: string, label: string) =>
  buildPrepareSnapshotPayload({
    shop: "launchbox",
    objectId,
    platform: "windows",
    hostname: "e2e",
    snapshotHash: sha256(label),
    baseVersion: 0,
    customPathRawPaths: [],
    variants: VARIANTS as never,
    files: filesFor(content) as never,
  });

/** Prepare, upload the one blob, commit; the way the launcher's sync does. */
const commit = async (objectId: string, content: string, label: string) => {
  const prepare = await api(
    "POST",
    "/profile/cloud-saves/prepare-snapshot",
    payloadFor(objectId, content, label)
  );
  assert.equal(prepare.status, 200, JSON.stringify(prepare.body));

  const prepared = validatePrepareResponse(prepare.body);
  for (const file of prepared.files) {
    if (file.status !== "upload") continue;
    const response = await fetch(file.uploadUrl, {
      method: "PUT",
      headers: file.requiredHeaders,
      body: content,
    });
    assert.equal(response.status, 200, "the blob uploads");
  }

  const committed = await api("POST", "/profile/cloud-saves/commit-snapshot", {
    pendingSnapshotId: prepared.pendingSnapshotId,
  });
  assert.equal(committed.status, 200, JSON.stringify(committed.body));
  return committed.body as { snapshotId: string; version: number };
};

/** The listing the launcher asks for, with the query it builds. */
const list = async (shop: "launchbox", objectId: string) => {
  const query = new URLSearchParams(toCloudSaveApiGame(shop, objectId));
  const response = await api(
    "GET",
    `/profile/cloud-saves/snapshots?${query.toString()}`
  );
  assert.equal(response.status, 200);
  return (response.body as unknown[]).map(validateRemoteSnapshotSummary);
};

describe("a recompilation's cloud saves against a running server", () => {
  const emulator = {
    content: "emulator save\n",
    id: "",
  };
  const port = {
    content: "pc port save\n",
    id: "",
  };

  it("is sent the way 4.4.0 sends it: the LaunchBox id, and the slug beside it", () => {
    const sent = payloadFor(PORT, port.content, "port") as unknown as Record<
      string,
      unknown
    >;

    assert.equal(sent.shop, "launchbox");
    assert.equal(sent.objectId, "1234", "not the composite key");
    assert.equal(sent.recompilationSlug, "soh");

    const plain = payloadFor(PLAIN, emulator.content, "plain") as unknown as {
      recompilationSlug?: string;
    };
    assert.equal("recompilationSlug" in plain, false);
  });

  it("advertises what the launcher requires before it syncs a port", async () => {
    const response = await fetch(`${BASE}/capabilities`);
    const { features } = (await response.json()) as { features: string[] };

    for (const [shop, objectId] of [
      ["launchbox", PORT],
      ["launchbox", PLAIN],
    ] as const) {
      for (const needed of getCloudSaveV2FeaturesForShop(shop, objectId)) {
        assert.ok(features.includes(needed), `${objectId} needs ${needed}`);
      }
    }
  });

  it("keeps a port and the game it ports as two saves, neither moving the other", async () => {
    const first = await commit(PLAIN, emulator.content, "plain");
    const second = await commit(PORT, port.content, "port");

    /* Sharing a key, the port's prepare at baseVersion 0 would have found the
       emulator's version 1 and been refused as a conflict. */
    assert.equal(first.version, 1);
    assert.equal(second.version, 1);
    assert.notEqual(first.snapshotId, second.snapshotId);

    emulator.id = first.snapshotId;
    port.id = second.snapshotId;

    const plainList = await list("launchbox", PLAIN);
    assert.deepEqual(
      plainList.map((snapshot) => snapshot.id),
      [emulator.id]
    );

    const portList = await list("launchbox", PORT);
    assert.deepEqual(
      portList.map((snapshot) => snapshot.id),
      [port.id]
    );
  });

  it("hands the slug back so the launcher's strict validator rebuilds the port's id", async () => {
    const portManifest = await api(
      "GET",
      `/profile/cloud-saves/snapshot-restore-manifest?snapshotId=${port.id}`
    );
    assert.equal(portManifest.status, 200);

    /* The validator accepts `recompilationSlug` on this object and nowhere else,
       and rebuilds the composite key from it. */
    const validated = validateRestoreManifest(portManifest.body);
    assert.equal(validated.snapshot.shop, "launchbox");
    assert.equal(validated.snapshot.objectId, PORT);
    assert.equal(portManifest.body.snapshot.objectId, "1234");
    assert.equal(portManifest.body.snapshot.recompilationSlug, "soh");

    const plainManifest = await api(
      "GET",
      `/profile/cloud-saves/snapshot-restore-manifest?snapshotId=${emulator.id}`
    );
    const plain = validateRestoreManifest(plainManifest.body);
    assert.equal(plain.snapshot.objectId, PLAIN);
    assert.equal(
      "recompilationSlug" in plainManifest.body.snapshot,
      false,
      "a plain game's manifest is what it always was"
    );
  });

  it("deletes the port's saves with the launcher's own URL, and only those", async () => {
    const url = buildDeleteGameCloudSaveSnapshotsUrl(PORT, "launchbox");
    assert.match(url, /recompilationSlug=soh/);

    const removed = await api("DELETE", url);
    assert.equal(removed.status, 204);

    assert.deepEqual(await list("launchbox", PORT), []);
    assert.deepEqual(
      (await list("launchbox", PLAIN)).map((snapshot) => snapshot.id),
      [emulator.id],
      "the emulator's save is still there"
    );
  });
});
