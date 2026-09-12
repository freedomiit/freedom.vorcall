# Security Policy

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Report it privately through GitHub's
[**Report a vulnerability**](https://github.com/freedomiit/freedom.vorcall/security/advisories/new)
form. That opens a private advisory only the maintainers can see, and it is the only
channel we watch for this.

Please include:

- What the problem is and what an attacker gets out of it
- The steps to reproduce it, and the version or commit you saw it on
- Whether it needs an account, an invite, or the server's door key

You will get an acknowledgement within a few days. Vorcall is maintained by a small team,
so a fix may take longer than that, but you will be told where it stands. If you would like
credit in the release notes, say so.

## Supported versions

Only the latest release gets fixes. There are no maintained older branches.

## Scope

**In scope** — anything that lets someone:

- Read, send or delete messages they should not have access to
- Reach a channel their resolved permissions deny
- Escalate privileges, take over another account, or forge a session
- Bypass a ban, a disable, or the invite requirement
- Read another person's voice or screen-share media
- Read or write arbitrary files on the server, or run code on it

**Out of scope:**

- Anything requiring the server's door key or admin key, which are secrets the operator
  hands out on purpose
- Denial of service through sheer volume against a server you do not run
- Findings in a self-hosted deployment that come from its own misconfiguration — a
  plain-HTTP server on the internet, a reverse proxy without body limits, a database exposed
  to the network. [docs/self-hosting.md](docs/self-hosting.md) covers how to avoid these.
- Missing hardening that has no exploit behind it
- Reports from automated scanners with no demonstrated impact

## What Vorcall does and does not protect

Being clear about this up front, so nobody reports a design decision as a bug:

- **Messages are not end-to-end encrypted.** The server stores them and can read them. The
  trust model is that you run the server, or you trust whoever does.
- **Voice and screen-share media are encrypted in transit** with a per-session key and
  relayed by the server. The relay does not decrypt media, but it is not an E2E guarantee
  either — the server issued the key.
- **The door key is a shared secret, not an identity.** Everyone who connects to a server
  has the same one. It keeps strangers off the API; accounts are what identify people.
- **The permission engine on the server is the boundary.** The client mirrors it to hide and
  disable things in the UI, and that mirror is never trusted for enforcement.
- **A transport without TLS is readable.** The client will connect over plain HTTP because
  that is genuinely useful on a LAN or a VPN, but nothing about it is confidential.

## For operators

If you run a Vorcall server:

- Put TLS in front of it. [docs/self-hosting.md](docs/self-hosting.md) has nginx and Caddy
  configs.
- Keep the API on loopback and let the reverse proxy reach it. Only UDP 5005 needs to be
  publicly reachable.
- Back up the `data` volume with the database — it holds the door key and the token signing
  key.
- Rotating the token signing key signs everyone out within 15 minutes. Rotating the door key
  locks out every client until they are given the new one.
- `/metrics` and `/api/admin/*` are served only to loopback and private-range sources and
  answer 404 to anything else. Keep it that way.
