# Cloud Save V2 end-to-end check

`cargo test` covers the wire shapes in isolation. This harness goes further: it
drives a **running** server through a full sync and validates every response
with the launcher's *own* validators, imported straight from the Hydra checkout.
If upstream tightens `cloud-save-contract.ts`, this catches it.

It is not part of `cargo test` — it needs a live server and a Hydra checkout —
so run it by hand when touching `src/cloud_saves.rs` or `src/storage.rs`.

## Running it

1. Start a stub of the official Hydra API (it only has to answer
   `/profile/me`, which is how this server validates launcher tokens — the
   bearer token becomes the user id, and `bad` is rejected):

   ```bash
   node tests/e2e/stub-official-api.mjs
   ```

2. Start the server against that stub, on a scratch data directory:

   ```bash
   HYDRA_SERVER_BIND=127.0.0.1:8799 \
   HYDRA_SERVER_PUBLIC_URL=http://127.0.0.1:8799 \
   HYDRA_SERVER_DATA_DIR=/tmp/hydra-e2e \
   HYDRA_OFFICIAL_API_URL=http://127.0.0.1:9911 \
   HYDRA_SERVER_SECRET=test-secret-for-e2e \
   cargo run
   ```

3. Copy the test into the launcher's cloud-save directory and run it there —
   the relative imports resolve against the launcher's own modules:

   ```bash
   cp tests/e2e/cloud-save-v2.test.ts \
     ../hydra/src/main/services/cloud-save/__e2e.test.ts
   cd ../hydra
   node --import ./scripts/register-ts-node.mjs \
     --test src/main/services/cloud-save/__e2e.test.ts
   rm src/main/services/cloud-save/__e2e.test.ts
   ```

Start from an empty `HYDRA_SERVER_DATA_DIR`: the first test uploads at
`baseVersion: 0`, which only holds when the game has no snapshot yet.

`cloud-save-state-metadata.test.ts` runs the same way, against the same server
(it uses its own game and user). It checks that an emulator save state's
`stateMetadata` survives the round trip — needed from launcher 4.1.6, whose
validators reject any key they don't expect on the files they read back.

## What it covers

- `prepare-snapshot` responses pass `validatePrepareResponse`, including the
  base64-of-raw-digest `x-amz-checksum-sha256` header
- identical blobs under different identities share one upload URL
- `snapshots` passes `validateRemoteSnapshotSummary`
- the restore manifest passes `validateRestoreManifest`
- downloaded bytes hash back to the manifest hash
- unchanged files come back as `skip` on re-upload
- a stale `baseVersion` is refused with 409 rather than clobbering
- snapshots and blobs are scoped per user
- an upload whose bytes don't match the declared SHA-256 is refused
- committing without uploading fails
- deleting a save garbage collects its blobs
