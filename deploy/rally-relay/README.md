# Rally room relay

Macs and unattended workers connect outbound to one HTTPS endpoint. Each project room has its own SQLite-backed Durable Object. The relay stores handoffs, assigns each item to one eligible principal, and retains acknowledgments and results across reconnects. The `envelope` field uses the Rally v1 delivery fields. The poll client runs independently of the local Rally daemon.

## Run the acceptance test

Use Node 22.22 or newer, npm, Git and Python 3.9 or newer. From this directory run `npm ci && npm test`. The test starts and stops Wrangler locally, uses disposable credentials and storage, restarts the relay between send and claim, races two clients, and checks a real Git revision in a separate worker process.

## Enroll and run

Assign a separate random token to each agent session that needs individually addressed receipts. Give a machine-level worker its own token. One enrollment identifies exactly one principal; use unique IDs. The room ID can be shared across different repository paths. Restrict enrollment files and journals to the account running the client.

Generate a token on the client with `python3 -c 'import secrets; print(secrets.token_urlsafe(32))'` and store it in a secret manager. Supply its value to the client as `RALLY_RELAY_TOKEN`. Obtain the enrollment key with `python3 -c 'import hashlib,os; print(hashlib.sha256(os.environ["RALLY_RELAY_TOKEN"].encode()).hexdigest())'`. Give the relay the following JSON as its `ENROLLMENTS` secret, replacing each example key with a distinct digest.

```json
{
  "<sender digest>": {"id":"macbook:lead","rooms":["org:project"],"capabilities":["handoff"]},
  "<agent digest>": {"id":"studio:agent-1","rooms":["org:project"],"capabilities":["handoff"]},
  "<worker digest>": {"id":"cloud:verify","rooms":["org:project"],"capabilities":["verify"]}
}
```

For local use, place the JSON on one line in a private `.dev.vars` file as `ENROLLMENTS='<json>'` and run `npm run dev`. To host, select the intended Cloudflare account, deploy this directory with `npx wrangler deploy`, then load the registry via `npx wrangler secret put ENROLLMENTS`. The endpoint denies access until the registry exists. To revoke a device, remove its digest and replace the secret. For rotation, replace the digest while keeping the same principal ID.

On each client set `RALLY_ROOM=org:project`, `RALLY_RELAY_URL` to the relay origin and `RALLY_RELAY_TOKEN` to that principal's token. HTTP is supported on loopback for local tests.

```sh
# MacBook
python3 client.py send review-123 studio:agent-1 'Review the Projects change'
python3 client.py status review-123
python3 client.py members

# In the addressed Studio agent session, after wake
python3 client.py wait
python3 client.py ack review-123
python3 client.py complete review-123 'Reviewed the Projects change'
```

Reuse the event ID when retrying a send. Claim reserves one item and returns it again until completed. Only its owner can acknowledge or complete it. A session explicitly acknowledges after reading the item. Membership shows the last contact time. A disconnected session keeps its claim, so a sleeping agent can resume without a competing run. An abandoned claim currently requires operator investigation. Run `wait` inside the enrolled agent session to poll across transient disconnects and return the next handoff. Use `claim` for one poll.

## Unattended execution

On an always-on Linux or macOS worker with read access to the approved Git source, run a foreground worker under your process supervisor. Keep the SQLite journal on persistent disk. The handler executable, working environment, and repository source are chosen by the worker operator. The task supplies a full commit SHA.

```sh
umask 077
python3 client.py work --journal ./verify.sqlite --capability verify -- python3 verify_revision.py /srv/git/project
```

Submit from a Mac before sleep. Use a full commit SHA available in the worker's repository.

```sh
python3 client.py send nightly-123 '*' "$COMMIT_SHA" --capability verify
python3 client.py status nightly-123
```

The verification handler checks out the requested commit in a temporary clone and runs Git's whitespace/error check. The worker returns its exit code and the first 2,000 output bytes. Replace the handler with a locally configured executable to support another job. A crash after execution starts and before the result reaches the journal leaves the run pending for manual inspection. Keep each journal with its worker. Relay credentials, repository access, and handler privileges are configured independently.
