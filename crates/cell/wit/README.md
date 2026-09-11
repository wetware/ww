# Cell WIT interfaces

This directory contains custom host imports for production Cells.

## P3 transport

`p3/transport.wit` defines `wetware:transport/connection@0.2.0`. The interface
grants one ordered bidirectional byte connection. `crates/cell/src/p3.rs`
implements the host side. `std/system` supplies the guest binding.

The transport has no address selection, socket operation, pollable, scheduler
state, or explicit flush function. Each direction uses a bounded 64 KiB host
buffer. Guest-visible write completion waits for the host flush.

`p3/fixture.wit` and `p3/host.wit` extend the transport only for native P3 host
regressions. Production guests do not import the fixture interface.

## Routing-key import

The canonical routing-key WIT source lives in
`crates/guest/routing-key/wit/key.wit`.

The package is `wetware:routing@0.1.0`. Its `derive` function returns the
canonical provider-routing CID for caller-supplied bytes. The function grants
no object capability or network authority.

`crates/cell/src/proc.rs` generates the host binding from the canonical source.
A component that does not import the interface receives no routing-key binding.

## PID0 readiness import

The canonical readiness WIT source lives in `std/kernel/wit/kernel.wit`. The
host installs `wetware:kernel-runtime/readiness@1.0.0` only for trusted PID0.
Ordinary Cell linkers omit the import.

## Linker policy

Production uses explicit P3 linker composition. The linker registers WASI CLI,
clocks, filesystem, and random packages plus the applicable Wetware imports.
It does not register WASI sockets or the deleted P2 streams interface.

Keep one canonical source for every WIT package. Guest bindings and host
bindings must resolve the same versioned definition.
