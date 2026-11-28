# Additional Caddy sites

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

`app` must be a service in this Compose project (or another name reachable on
the `paseo-relay` network). For static content, use `root` + `file_server` and
mount the content into the Caddy container.

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
