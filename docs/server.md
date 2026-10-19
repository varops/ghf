# Running ghf-server

`ghf-server` is the canonical store for checkpoints and traces. It is one binary plus a data directory. It holds no Git data.

## Data directory

| File | Contents |
|---|---|
| `server.db` | SQLite database holding traces, checkpoints, objects, tokens, retention policy and pending-client reports |
| `server-id` | Stable id for this server instance; clients pin it when they trust the server |

**Back up** with SQLite's online backup while the server runs:

```bash
sqlite3 /var/lib/ghf/server.db ".backup /backups/ghf-$(date +%F).db"
```

You can also stop the server and copy the directory. **Restore** by putting `server.db` and `server-id` back.

**Keep `server-id` with the database.** If it changes, every client refuses to talk to the server (`E_SERVER_MISMATCH`) until someone runs `ghf trust` again.

## Serve

```bash
ghf-server --data-dir /var/lib/ghf serve \
  --listen 0.0.0.0:7878 \
  --tls-cert /etc/ghf/cert.pem --tls-key /etc/ghf/key.pem
```

**TLS.** Clients require `https://` for any host other than localhost. You have two options:
- Pass `--tls-cert` and `--tls-key` (PEM), and the server terminates TLS itself.
- Run it on loopback behind a reverse proxy that terminates TLS.

**Settings.** Every flag also reads an environment variable:

| Setting | Flag | Variable |
|---|---|---|
| Data directory | `--data-dir` | `GHF_DATA_DIR` |
| Listen address | `--listen` | `GHF_LISTEN` |
| TLS certificate | `--tls-cert` | `GHF_TLS_CERT` |
| TLS key | `--tls-key` | `GHF_TLS_KEY` |

The server logs to stderr.

## Tokens

Clients authenticate with bearer tokens. Each token is scoped to one or more repositories.

```bash
ghf-server --data-dir /var/lib/ghf token create --name fleet --repos 'acme/*'
ghf-server --data-dir /var/lib/ghf token create --name alice --repos 'acme/api,acme/web'
ghf-server --data-dir /var/lib/ghf token list
ghf-server --data-dir /var/lib/ghf token revoke --name alice
```

**Scope syntax:**
- `owner/name` matches one repository.
- `owner/*` matches every repository of an owner.
- `*` matches everything.

**Read-only tokens:** add `--read-only` to `token create`. Such a token can fetch objects, traces, checkpoints and policy, but every route that changes state refuses it with `E_AUTH`: object uploads (including `objects/missing`), trace and checkpoint pushes, holds, pending reports and prune. Use one for the browser viewer and for auditors. `ghf sync` needs a read-write token, because it reports pending traces on every run.

```bash
ghf-server --data-dir /var/lib/ghf token create --name audit --repos 'acme/*' --read-only
```

**Storage:** the server stores only a BLAKE3 hash of each token. `create` prints the token once. `token list` shows whether each token is `read-only` or `read-write`.

**On clients,** set `GHF_TOKEN`, or run `echo "$TOKEN" | ghf login`. `login` saves the token to `~/.config/ghf/credentials.toml` with mode 0600.

## Retention

**When a trace becomes eligible for pruning:**
- **Finalized traces:** `--after-finalize` after finalization. The default is `30d`.
- **Closed traces that are never finalized:** `--unfinalized-cap` after closing. The default is `90d`. Clients warn 7 days before the cap.
- **Open traces:** never.

**Pruning skips:**
- Traces with an active hold, including `pin`.
- Traces that any client has reported as having unsynced checkpoints.
- Checkpoints that a retained fork was created from. These are kept so the fork stays restorable.

**Per-repository overrides:**

```bash
ghf-server --data-dir /var/lib/ghf policy acme/api --after-finalize 14d --unfinalized-cap 60d
```

**Prune runs:**
- **Automatic:** the server prunes every hour. Change this with `--prune-interval`; `0s` disables it.
- **By a client:** `ghf prune` runs it for that client's repository.
- **By hand:**

```bash
ghf-server --data-dir /var/lib/ghf prune --dry-run
```

**Garbage collection:** after pruning, objects that no retained checkpoint references are deleted. Objects younger than an hour are kept, because an upload may still be in flight. `GHF_GC_GRACE` changes that window, for example `GHF_GC_GRACE=10m`. Pruning never touches Git.

## Viewing traces in a browser

The server has a read-only web viewer at `/ui`, for example `https://ghf.example.com/ui`. It shows what agents tried:

- **Repository:** every trace with its state, writers, checkpoint count, last activity and final commit.
- **Trace:** a timeline of checkpoints with time, author, message, annotations and changed files. `tests=pass` and `tests=fail` are highlighted. Forks link to the checkpoint they started from.
- **Diff:** a checkpoint against its parent, or any two checkpoints. Binary files are noted, not shown.
- **Files:** the tree and any file as it was at a checkpoint.

**Sign in** by pasting a token once. Create a read-only one for this. The browser keeps it in an `HttpOnly`, `SameSite=Strict` cookie for 30 days, marked `Secure` when the server terminates TLS or a proxy sends `X-Forwarded-Proto: https`. The viewer shows only the repositories the token's scope covers, and it has no write actions.

**Links are stable** and safe to share with anyone who has a token for the repository:

| Page | URL |
|---|---|
| Traces | `/ui/{owner}/{name}` |
| Timeline | `/ui/{owner}/{name}/traces/{trace_id}` |
| Checkpoint and its diff | `/ui/{owner}/{name}/checkpoints/{checkpoint_id}` |
| Diff of two checkpoints | `/ui/{owner}/{name}/diff?from={a}&to={b}` |
| File at a checkpoint | `/ui/{owner}/{name}/checkpoints/{checkpoint_id}/files/{path}` |

Pruned checkpoints and their content are gone, so links to them return 404.

## API

HTTP/JSON. Repository routes live under `/v1/repos/{owner}/{name}/`. See [`proto/src/api.rs`](../proto/src/api.rs) for the route table and types.

- **Health check:** `GET /v1/info` needs no authentication. Use it for health checks.
- **Content:** objects are zstd-compressed and content-addressed with BLAKE3. The server verifies every upload against its id.
- **Checkpoint check:** a checkpoint is accepted only when its whole manifest closure is present.
