# accesskit_unix 0.13.1, patched for mesh-kvm

This is accesskit_unix 0.13.1 as published on crates.io (upstream commit 405d578, `platforms/unix`
in https://github.com/AccessKit/accesskit), with LICENSE-MIT and LICENSE-APACHE from that commit.
Cargo.toml's `[patch.crates-io]` makes the build use it in place of the registry copy.

## Why

In 0.13.1 the accessibility thread ran `run_event_loop(...).await.unwrap()`, and the loop returned
an error whenever the AT-SPI bus couldn't be reached: at start when `org.a11y.Status.IsEnabled` is
true, again whenever IsEnabled changes, and whenever talking to the bus failed. With mesh-kvm's
`panic = "abort"` that ended the whole program. It was seen as
`Handshake("Server GUID mismatch: ...")` after the desktop's AT-SPI socket had been re-bound by
another daemon.

No 0.13.x release fixes it; later upstream releases go with newer accesskit and egui.

## What changed

Only `src/context.rs`, and a test-only dev-dependency in `Cargo.toml`. Every change is marked
`MESH-KVM PATCH`.

- The thread no longer unwraps the loop's result. An error prints
  `accessibility is off: <error>` to stderr and the thread ends. The channel's senders all ignore a
  closed channel (`let _ = try_send(...)`), so the adapters carry on with accessibility off.
- In the loop, a failed `IsEnabled` read or a failed `Bus::new` leaves the bus off, prints the same
  line, and the loop carries on. The next IsEnabled change tries again.
- A failed register, unregister or event emit lets go of the bus and deactivates the adapters,
  just as IsEnabled turning false does, prints the same line, and the loop carries on.

When the bus works, nothing is different.

The test `mesh_kvm_patch_tests` (`cargo test -p accesskit_unix`) uses no D-Bus daemon: a private
peer-to-peer connection stands in for the session bus, and AT_SPI_BUS_ADDRESS points at a server
with the wrong GUID, and at a socket nobody listens on, in a directory of the test's own.
