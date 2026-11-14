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
Automatic native replacement supports managed GNU ARM64 Linux/systemd only;
qualification and production activation remain separate gates. Ordinary startup
requires no native-upgrade admission on any supported Host platform. Register a
service only when requested, then use the native manager for explicit activation.
See `doc/installation.md` for prerequisites and recovery.

### If installed

Check if setup is complete: `ww doctor`

Look at the "Install state" section. Missing daemon registration is valid for a
foreground installation. Use `ww install --release <RECORDED_ROOT>` to authenticate
the exact recorded package and restore missing owned software. Bare `ww install`
requires the running executable and existing FHS to match the ownership receipt.
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
