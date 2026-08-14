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

**macOS (build from source):**
```
git clone https://github.com/wetware/ww.git && cd ww
make all
ww perform install
```

**Linux (pre-built binary):**
```
curl -LO https://github.com/wetware/ww/releases/latest/download/ww-linux-x86_64
chmod +x ww-linux-x86_64
sudo mv ww-linux-x86_64 /usr/local/bin/ww
ww perform install
```

`ww perform install` creates `~/.ww`, generates an identity,
and registers the background daemon. It prints next steps.

### If installed

Check if setup is complete: `ww doctor`

Look at the "Install state" section. If the identity or daemon is missing,
run `ww perform install` to fix it.

If everything is OK, hand off to the quickstart.

## After setup

Recall what brought them here (from your first question) and route:

- Building something specific -> `/ww-build-app`
- Exploring / curious -> `/ww-concepts` or `/ww-examples`
- Not sure -> present a slimmed menu:
  1. Build an app -- `/ww-build-app`
  2. Review an app -- `/ww-review-app`
  3. Deep dive -- `/ww-concepts`, `/ww-examples`, `/ww-reference`
