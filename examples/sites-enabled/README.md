# Additional Caddy sites

These commands assume `examples/.env` exists; copy it from
`examples/.env.example` once and set `RELAY_DOMAIN`.

`examples/Caddyfile` loads every `*.caddy` file in this directory:

```caddyfile
import /etc/caddy/sites-enabled/*.caddy
```

To add another domain, create one file per site here — do not edit the relay
block in `examples/Caddyfile`:

```caddyfile
# examples/sites-enabled/app.caddy
app.example.com {
    reverse_proxy app:3000
}
```

`app` must be reachable under that name:

- **Same Compose project:** add the service to
  `examples/docker-compose.caddy.yml`; it joins the default `paseo-relay`
  network automatically.
- **Another Compose project:** declare the external network and attach the
  backend service (and Caddy, if it is also there):
  ```yaml
  networks:
    paseo-relay:
      external: true
  services:
    app:
      networks: [paseo-relay]
  ```
- **A process on the host:** a Caddy container cannot use `127.0.0.1` for it.
  Add `extra_hosts: ["host.docker.internal:host-gateway"]` to the Caddy service
  and use `reverse_proxy host.docker.internal:3000`; alternatively run Caddy on
  the host and use `127.0.0.1:3000`.
- **Static content:** mount the content directory into the Caddy container and
  use `root` + `file_server`.

These files are imported by the bundled Docker Caddy. If Caddy runs directly on
the host, keep the equivalent site files in that Caddy's own configuration and
use `127.0.0.1:4000` for the relay (see the quickstart host-Caddy section).

The DNS record for the new name must point at the VPS and TCP 80/443 must be
reachable. Caddy obtains the certificate automatically. Validate and reload
without dropping WebSocket sessions:

```sh
docker compose -f examples/docker-compose.caddy.yml exec caddy \
  caddy validate --config /etc/caddy/Caddyfile
docker compose -f examples/docker-compose.caddy.yml exec caddy \
  caddy reload --config /etc/caddy/Caddyfile
```

If validation fails, Caddy keeps serving the previous configuration. The
`import` glob logs a warning when no extra files exist; that is harmless.

If the existing Caddy is not the one from this example, use the equivalent
import/site-file mechanism in that deployment instead; the relay site block is
in `examples/Caddyfile` for reference.
