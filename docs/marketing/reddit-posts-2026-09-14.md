# Reddit — Vorcall, 2 posts

Owner: the founder · Written: 2026-09-14 · Source of truth for claims: `README.md`, `PROTOCOL.md`, `docs/self-hosting.md`.

## How to use this file

- **Order.** r/SideProject first, then r/MadeThis two or three days later. The two subs share readers; posting both in one day reads as a spam run even when the posts differ.
- **Account.** Post from a personal account with real history. Answer every comment in the first hour — on a self-hosted project the thread will be "how do I run it", "why not Matrix" and "is it encrypted", and those answers are the post.
- **Links.** Both subs allow a link in the body. The link is the repo, not a landing page: `https://github.com/freedomiit/freedom.vorcall`. There is no signup and nothing to buy, and saying so early is what keeps the thread friendly.
- **Register.** Write it the way you'd write it to a friend. Short paragraphs, contractions, no bold headers, no code blocks, no feature table. Say "2GB" not "2 GiB", "Mac" not "macOS", "no Electron" not the framework name. One or two mechanism words are fine as flavour — more than that and it reads like a README someone pasted into Reddit. No adjective about Discord: never "better than", never "Discord killer". Vorcall is smaller than Discord on purpose and the post should say so before a commenter does.
- **Facts you may state.** Self-hosted, invite-only, one server per deployment. Native desktop app in Rust, Linux/Windows/Mac, not Electron. Text and voice channels under categories, plus two-person DMs. Push-to-talk or voice activation, echo cancellation, noise suppression, per-person volume, priority speaker, join/leave/mute/deafen sounds. Screen share of a monitor or a single window with its audio. A shared soundpad. 23 permissions, roles with colours, icons and a hierarchy, overridable per channel and per member. Replies, edits, reactions, attachments of any type up to 2GB, anything larger streamed straight off the sender's machine while they're online. Profiles, unread and mention counts, `@everyone` and `@here`. Kick, ban, server mute and deafen, move, invite revocation. One `docker compose up -d`, about 1GB of memory. The app updates itself from the server it's connected to. MIT.
- **Facts you may not state.** Any user count, any uptime figure, any latency or bandwidth benchmark, any claim that it's been run at scale, any length of time it took to build. It runs a friend group's server. Say exactly that when asked.
- **Every post has one differentiator up front and one honest weakness.** The weaknesses are real and load-bearing: no mobile app, no web UI, no search, no threads, no group DMs, no federation, no bots — and it is not end-to-end encrypted. The relay decrypts to fan out, and whoever runs the server can read the messages. Put it in the post yourself, in plain words. A self-hosting thread that catches you soft-pedalling encryption is over.

---

## 1. r/SideProject · link allowed

**Title:** My friend group has been on the same Discord for years and none of it is ours, so I built us our own

**Body:**

Every call we've ever had, every dumb inside joke, the whole thing lives on someone else's server and someone else decides whether it stays. That bugged me for a long time and eventually I did something about it.

It's called Vorcall. Basically a tiny Discord for exactly one friend group. You run the server, you hand out invites yourself, and that's it — there's no browsing communities, no join button, nobody else on the box.

What it does:

- text and voice channels grouped into categories, plus DMs
- voice with push to talk or voice activation, echo cancellation, noise suppression, a volume slider per person
- screen share, a whole monitor or just one window, with the audio
- a soundpad, which my friends had ruined roughly a day after the first call worked
- roles and permissions with colors and per-channel overrides
- replies, edits, reactions, unread counts, @everyone and @here
- attachments up to 2GB, and anything bigger just streams off your own machine instead of taking up space on the server

The two things I actually care about:

The app is native. Rust, no Electron, no browser hiding inside it. One binary on Linux, Windows and Mac, and it doesn't sit there eating a gig of RAM while you're idle in a voice channel.

And I wrote the voice part from scratch instead of gluing something in. That took about four times longer than I expected and I'd do it again, because calls sound good and that was the entire point of the project.

Installing it is one docker compose command and then reading a key out of the logs. Runs in about 1GB of RAM on whatever spare box you've got.

Now the bad parts, so nobody has to go find them:

It's not end to end encrypted. Whoever runs the server can read the messages. The point was "our hardware instead of theirs", not "I don't trust myself".

No phone app and no web version. You're at a desktop or you're not in the chat.

No message search, no threads, no group DMs, no bots, no plugins, and one deployment is one server, that's all.

On Linux it needs PipeWire or the app won't even open. On Mac and Windows the builds aren't properly signed yet so the first launch gives you the scary warning.

It's MIT, all of it: https://github.com/freedomiit/freedom.vorcall

One thing I'm genuinely unsure about. I built it with no concept of joining or leaving at all — everyone with an account is just in the server, permanently, and channels appear or don't based on permissions. Made a lot of things simpler. But it also means a deployment is one group forever. If you've self hosted chat for friends before, did you ever actually want more than one server out of it?

---

## 2. r/MadeThis · link allowed

**Title:** I made a voice and text chat for my friend group so we'd stop renting one from someone else

**Body:**

It's a private chat you run yourself. Think a small Discord that nobody outside can get into, because there's no join button at all — just invites you hand out one at a time.

The bit I'm happiest with is that the desktop app is actually native, written in Rust with no Electron in it. It opens instantly and doesn't sit there chewing a gig of RAM while you're in a call.

Voice works how you'd want: push to talk or voice activation, echo cancellation, noise suppression, a volume slider for each person so you can turn down the one friend who's always too loud. Screen share with audio, either a full monitor or a single window. There's a soundpad too, which was a mistake.

Text side is the normal stuff — channels and categories, DMs, replies, edits, reactions, unread counts, roles and permissions you can override per channel. Attachments up to 2GB, and bigger files stream straight off your machine so the server never stores them.

Running it is one docker compose command. The app also updates itself from whatever server it's connected to, so nobody in the group has to keep downloading things.

What it isn't: no phone app, no web version, no search, no threads, no bots, and it's not end to end encrypted — if you run the server you can read the messages. It's about owning the hardware, not hiding from yourself.

MIT, source here: https://github.com/freedomiit/freedom.vorcall

Happy to answer anything. Echo cancellation and getting screen capture working on all three operating systems were easily the worst parts.

---

## 3. r/selfhosted · link allowed · flair: Release

**Note on the sub.** It's r/**selfhosted** — r/selfhosting is a much smaller lookalike, don't post there by mistake. Reddit blocks scraping so the rules below came from a third-party guide and the sub's known norms rather than the rules page itself; **read the sidebar yourself before posting.** What matters:

- **Flair is required.** Use `Release` (or `Product Announcement` if that's what the picker offers). An unflaired post gets removed by automod, not by a human.
- **Self-promo is allowed in context** — you're the dev, say so in the first line. What gets removed is a link with no substance, and anything that isn't genuinely self-hostable. Vorcall is fine on both counts.
- **Deployment info goes up front, not at the bottom.** This sub wants the compose, the ports, the RAM, the volumes and the backup story before it wants the feature list. That is the opposite order from r/SideProject.
- **They will ask, every time:** how is this different from Matrix/Element, Mumble, Revolt, Rocket.Chat? Does it phone home? Is it actually open source or open-core bait? ARM? Reverse proxy? Is it end-to-end encrypted? Answer all of those *in the post*.
- **Attach screenshots.** It's a GUI app and this sub scrolls past announcements with no picture. Two or three: a channel with messages, a voice channel mid-call, the screen share stage.
- **Do not** call it a Discord alternative in the title and do not use any comparative adjective. Describe what it is; let the comments make the comparison.

**Title:** Vorcall — self-hosted voice and text chat for one private group. Native desktop client, one docker compose, MIT.

**Body:**

I'm the developer. I wrote this because my friend group's whole history lives on someone else's server and I wanted it on mine instead.

It's a chat server plus a desktop client for a group that already knows each other. Invite-only, one server per deployment, no directory, no federation, nobody else on the box.

Deployment first, since that's what matters here.

```
git clone https://github.com/freedomiit/freedom.vorcall
cd freedom.vorcall
docker compose up -d
```

That builds the backend, brings up Postgres next to it, runs the migrations and generates the door key and the JWT secret. Around 1GB of RAM. You read the door key out of the logs and make the first invite with a CLI command — the first account to register becomes the owner.

Ports: TCP 5000 for HTTP and the WebSocket, bound to loopback by default so you can put nginx and TLS in front of it. UDP 5005 for the voice relay, and that one has to be published directly — a reverse proxy can't carry it. If you're behind a NAT or a cloud firewall that's the one rule you have to open by hand. There's an nginx config and a host provisioning script in the repo for the whole production setup.

Volumes: Postgres data, uploaded attachments, and a small `data` volume holding the generated secrets — lose that one and the door key rotates and everyone gets signed out, so it's worth backing up alongside the database. There's a `pg_dump` backup script in the repo and the provisioning script installs it as a nightly cron.

On privacy, since it always comes up: the client talks to your server and nothing else. No analytics, no crash reporting to me, no phoning home. Even updates come from your own server — the client pulls a signed manifest from whatever instance it's connected to, so it never touches GitHub and you decide when your group gets a new version.

What it actually does: text and voice channels under categories, two-person DMs, roles and permissions with per-channel and per-member overrides, replies, edits, reactions, unread and mention counts, profiles, kick/ban/server-mute. Voice has push to talk or voice activation, echo cancellation, noise suppression, per-person volume and priority speaker. Screen share of a monitor or a single window with its audio. Attachments of any type up to 2GB, and anything bigger streams straight off the sender's machine rather than landing on your disk.

The client is native — Rust, no Electron — for Linux, Windows and Mac. That was most of the work and most of the point.

Honest about the limits:

**Not end to end encrypted.** Voice is encrypted to the relay, which decrypts it to fan it out, and messages sit in Postgres readable by whoever runs the server. The threat model is "my hardware instead of a company's", not "I don't trust my own box". If you need E2EE, you want Matrix.

Desktop only. No mobile app and no web UI at all, so there's nothing to reach from a phone.

No message search, no threads, no group DMs, no bots, no plugins, and one deployment is exactly one server.

Released binaries are Linux x86_64, Windows x86_64 and Apple Silicon. The backend images are multi-arch so it should build fine on arm64, but I've only actually run it on x86_64 — a Pi could probably host it, you'd just have to build the client yourself for an ARM desktop.

Linux clients need PipeWire as a hard dependency. No PipeWire, the app doesn't start.

How it compares, briefly, since someone will ask: Matrix is federated, general-purpose and does E2EE, and it's a much bigger thing to run — Vorcall is one server for one group and nothing else. Mumble is excellent at voice and isn't trying to be a chat app with history, roles and file uploads. The Discord-shaped projects I know of are web apps; this one is a native desktop client, which is the specific thing I wanted and couldn't find.

MIT, genuinely — no open-core tier, nothing held back, no paid version to upsell you to.

https://github.com/freedomiit/freedom.vorcall

Happy to answer anything about the deployment or the voice path. If you try it and the reverse proxy or the UDP port gives you trouble, tell me, because that's the part where my docs are least tested by anyone who isn't me.
