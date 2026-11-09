---
name: ww-onboard
description: First-time setup and orientation for new Wetware users
reads:
  - doc/ai-context.md
---

# Onboard New User

One-time setup and orientation for someone who just arrived.

## First: ask what brought them here

Before setup steps, find out what the user actually wants.  Ask
something like:

> Welcome to Wetware!  Before we set anything up -- what brought you
> here?  Are you exploring, building something specific, or just
> curious how it works?

Their answer tells you how deep to go and where to hand off after
setup.  Remember it -- you'll use it at the end.

## Check installation

Run `ww --version` to see if the binary is installed.

### If not installed

Tell them how to get started:

**Linux GNU ARM64 qualification candidate (authenticated release):**
Provision the release keyring independently. Set `RELEASE_CID` to the selected
signed release and use an independently obtained installer:

```sh
sh scripts/install.sh --release "$RELEASE_CID"
```

The installer requires Kubo and OpenSSL 3.x or 4.x. It validates the Host and
kernel/status FHS, preserves existing identity, and creates no daemon or namespace.
Automatic channel selection remains gated. Source builds can run with explicit
FHS and identity; they do not bypass authenticated installation.
Native acceptance remains pending. Other targets refuse guarded admission;
fresh guarded installation is unavailable on macOS. See `doc/installation.md`
for prerequisites and current release activation gates.

A fresh guarded installation requires stopped `ww startup admit` with the exact
intended inputs before its first run. Register a service only when requested.
Follow the guarded service recipe in `doc/installation.md`: persist explicit
executor and compiler worker counts, choose enablement while stopped, admit the
actual configuration, then start. Admission does not enable or start a service.
Existing unguarded installations keep their separate native activation path.

### If installed

Check if setup is complete: `ww doctor`

Look at the "Install state" section. Missing daemon registration is valid for a
foreground installation. Guarded repair requires
`ww install --release <RECORDED_ROOT>` with the exact authenticated package.
Bare `ww install` retains only the separate legacy exact-artifact repair path.
Neither repair upgrades the Host or FHS. A missing recorded identity requires restoring
the original identity from backup. Do not regenerate identity during repair.

If everything is OK, hand off to the quickstart.

## After setup

Recall what brought them here (from your first question) and route:

- Building something specific -> `/ww-build-app`
- Exploring / curious -> `/ww-concepts` or `/ww-examples`
- Not sure -> present a slimmed menu:
  1. Build an app -- `/ww-build-app`
  2. Review an app -- `/ww-review-app`
  3. Deep dive -- `/ww-concepts`, `/ww-examples`, `/ww-reference`
