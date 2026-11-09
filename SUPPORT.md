# Support

**English** | [简体中文](SUPPORT.zh-CN.md)

## Getting help

- **Documentation**: the searchable [docs site](https://kfxhjz.github.io/paseo-relay/),
  [README.md](README.md), [docs/](docs/), and the Chinese index at
  [docs/zh-CN/README.md](docs/zh-CN/README.md).
- **Deployment questions and community help**: use
  [GitHub Discussions](https://github.com/kfxhjz/paseo-relay/discussions)
  (Q&A / Show and tell).
- **Bug reports / feature requests**: use the GitHub issue templates. Include
  your relay version/commit, deployment topology, and redacted logs.
- **Security issues**: follow [SECURITY.md](SECURITY.md) — never open a public
  issue for a vulnerability.
- **Paseo client/daemon/upstream protocol questions**: use the
  [Paseo project](https://github.com/getpaseo/paseo) channels. This repository is
  a community relay, not the Paseo upstream.

## Support scope

This is a volunteer, community-maintained project. There is **no SLA, paid
support, or guaranteed response time**. Maintainers will do their best to
triage issues, but support is best-effort.

In scope:

- relay bugs, protocol behavior, resource/limits, deployment and upgrade
  guidance for the documented runtime contract;
- reproducible compatibility failures against Paseo's relay transport;
- security reports via the private channel.

Out of scope:

- operating your VPS/load balancer/Kubernetes cluster;
- Paseo client/daemon bugs or upstream feature requests;
- debugging custom modifications, forks, or unsupported multi-node topologies;
- handling abuse on your public relay (see [docs/operator-policy.md](docs/operator-policy.md));
- relay-level authentication or per-user quota features (see the non-goals in
  [DESIGN.md](DESIGN.md)).

## Before filing an issue

1. Reproduce on the latest release or `main`.
2. Run `paseo-relay --version` and `paseo-relay --check-config`.
3. Include `/ready`, relevant `/metrics` counters, and the relay's structured
   logs with `serverId`/`connectionId`/pairing links redacted.
4. If wire behavior changed, include the Paseo compatibility command/results.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and
[CONTRIBUTING.zh-CN.md](CONTRIBUTING.zh-CN.md). Good first areas are
documentation, deployment examples, compatibility tests, and fuzz/test coverage.
