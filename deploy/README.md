# Deploying Anwesen

Anwesen is a single read-only daemon. It binds `127.0.0.1:8080` by default and
trusts every request it accepts -- access control lives in front of it, not
inside it ([ADR-007]). A typical host runs the `anwesen.service` systemd unit
and an nginx (or caddy, or warpgate) reverse proxy as the network boundary.

## systemd

[`anwesen.service`](anwesen.service) runs the daemon as a dedicated `anwesen`
system user, restarts it on failure, and routes its structured stderr to the
journal.

```
cp target/release/anwesen /usr/local/bin/anwesen
useradd --system --no-create-home --shell /usr/sbin/nologin anwesen
cp deploy/anwesen.service /etc/systemd/system/
systemctl edit anwesen.service        # set the real vault path, see below
systemctl enable --now anwesen.service
journalctl -u anwesen -f
```

Point the unit at your vault with a drop-in (`systemctl edit anwesen.service`)
rather than editing the shipped unit:

```ini
[Service]
Environment=ANWESEN_VAULT=/srv/vault
ReadOnlyPaths=/srv/vault
```

The `anwesen` user needs read access to the vault and traverse (`x`) on its
directories -- grant it via group membership or directory permissions. Anwesen
never writes to the vault; the unit sets `ProtectSystem=strict` with no
writable paths, so any write attempt fails outright.

The unit drains in-flight requests on SIGTERM (systemd's default stop signal)
and is hardened for a service that writes nowhere and needs no privileges.

## Reverse proxy

[`nginx.example.conf`](nginx.example.conf) terminates TLS, routes by host, and
authenticates the client before any request reaches Anwesen. It offers HTTP
basic auth out of the box with mutual-TLS as a commented alternative. Adjust
`server_name`, the certificate paths, and the auth block, then reload nginx.

Anwesen never sees the proxy's auth: the proxy authenticates the client and
forwards to `127.0.0.1:8080`. Per [ADR-007] this is deliberate -- "if you reach
Anwesen, you may read everything it indexes."

### Warpgate

For off-host access the operator's reference pattern is warpgate ticketing:
a ticket-bearing client reaches warpgate, which forwards to Anwesen on
`localhost`. Anwesen does not see the ticket and needs no configuration for it
-- it is just another reverse proxy in front of the localhost bind.

[ADR-007]: the project's design vault, "ADR-007 Authentication Out of Scope".
