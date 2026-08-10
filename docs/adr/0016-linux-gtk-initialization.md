# ADR-0016: Linux GTK initialization for the tray

**Status:** Accepted
**Date:** 2026-08-04
**Deciders:** Solo project
**Depends on:** ADR-0015

## Context

`solo-tray` crashes on every launch on Linux:

```text
thread 'main' panicked at gtk-0.18.2/src/auto/menu.rs:29:9:
GTK has not been initialized. Call `gtk::init` first.
```

Reproduced on Ubuntu 24.04 against the installed `.deb`, in three
configurations: default (hardware EGL), `LIBGL_ALWAYS_SOFTWARE=1`, and
software plus `GDK_BACKEND=x11`. The EGL/MESA warnings present in the first
configuration disappear in the other two while the panic stays byte-identical,
so the GPU path is not the cause.

`tray-icon` 0.20 states its Linux contract plainly:

> On Windows and Linux, an event loop must be running on the thread — on
> Windows a win32 event loop and on Linux a gtk event loop. It doesn't need to
> be the main thread, but you have to create the tray icon on the same thread
> as the event loop.

The tray is built inside `SoloTrayApp::new` (`window.rs`), which eframe calls
on the main thread from `eframe::run_native`. eframe wraps winit, which talks
to X11/Wayland directly and never initializes GTK. Nothing else in the tray
process does either: `grep -r 'gtk::init' crates/solo-tray/src` returns
nothing. So `Menu::new()` hits gtk-rs's unconditional
`assert_initialized_main_thread!()` and aborts the process.

Windows and macOS are unaffected because `tray-icon` uses Win32 and
NSStatusItem there, which is why every Windows certification passed while the
Linux desktop entry (`Exec=solo-tray`) has never started.

The separate `solo-tray --desktop-window` subprocess is *not* affected: it uses
`tao` + `wry`, and `tao` is GTK-backed on Linux, so it initializes GTK itself.
The defect is specific to the tray process.

`Cargo.lock` resolves exactly one `gtk 0.18.2`, so binding to `gtk = "0.18"`
shares the same crate instance and global state that `muda` already uses.

## Decision

Initialize GTK on the main thread before `eframe::run_native`, and drive
pending GTK events from the existing eframe `update()` loop, on Linux only.

The tray keeps being created on the main thread, which is the same thread that
now runs GTK iterations, satisfying `tray-icon`'s "same thread" requirement
without changing tray ownership.

## Options Considered

### Option A: Initialize on the main thread, pump from `update()`

| Dimension | Assessment |
|---|---|
| Complexity | Low — one init call, one bounded pump, both `#[cfg]`-gated |
| Blast radius | Linux only; Windows/macOS compile unchanged |
| Ownership model | Unchanged — tray stays owned by the eframe app |
| Risk | GTK iteration cadence is tied to repaint cadence |

**Pros:** smallest diff; preserves the existing single-threaded tray ownership
and the existing muda dispatcher; no cross-thread messaging for icon updates,
which the app already performs from the eframe thread on health changes.

**Cons:** GTK only advances when `update()` runs. The repaint pump guarantees
≈4 Hz, so worst-case dispatch latency is ~250 ms.

### Option B: Dedicated GTK thread owning the tray

| Dimension | Assessment |
|---|---|
| Complexity | High — tray creation and every mutation move off the eframe thread |
| Blast radius | Restructures tray ownership on all platforms or forks it per-OS |
| Ownership model | Changed — needs a command channel for icon/health updates |
| Risk | Two long-lived UI loops; more failure modes to reason about |

**Pros:** a real `gtk::main()` loop, so dispatch latency is not coupled to
repaint cadence.

**Cons:** `tray-icon` requires the tray be created *and modified* on the GTK
thread. The app updates the tray icon for health and pulse animation from the
eframe thread today, so this forces a message-passing layer that exists purely
to satisfy Linux. It also diverges the Linux code path from Windows/macOS,
which is the opposite of what the current design optimises for.

### Option C: Drop the tray on Linux

| Dimension | Assessment |
|---|---|
| Complexity | Low |
| Blast radius | Product-visible feature removal |
| Ownership model | N/A |
| Risk | Ships a `.deb` whose desktop entry is the only entry point |

**Pros:** removes the failing surface outright.

**Cons:** the tray is how the daemon is supervised, restarted, and reached; the
Ubuntu package advertises tray/Desktop supervision. Removing it to avoid a
missing init call trades a product capability for an afternoon of work.

## Trade-off Analysis

Option B buys lower dispatch latency at the cost of restructuring tray
ownership across platforms. That cost is only justified if ~250 ms menu
latency is unacceptable, and it is not: the repaint pump was introduced for
exactly this reason and already governs how quickly forwarded menu events are
drained. The high-frequency menu items are additionally handled by
`spawn_menu_dispatcher`, which parks directly on muda's channel and is
unaffected by repaint cadence.

Option C is a real option only if the Linux tray proves unworkable on real
desktops. It has not been tried, because it has never started.

Option A is therefore the correct first move: it matches the documented
contract, keeps one code path across platforms, and is small enough to verify
end-to-end on a real Ubuntu host.

## Consequences

Easier: the Linux desktop entry starts; the tray, its menu, and the owned
Desktop window become reachable on Ubuntu for the first time.

Harder: GTK now shares the main thread with winit's event loop. If a future
change moves tray construction off the main thread, the init call must move
with it, and the pump must run on whichever thread owns the tray.

To revisit: if measured menu latency is poor on a real desktop session, or if
GTK and winit contend on Wayland, Option B becomes the fallback and this ADR
should be superseded rather than amended.

CI coverage: the Ubuntu runner now exercises the installed tray under Xvfb and
DBus. The guard was also checked against a deliberately crashing binary so it
cannot pass merely because the process exited early.

## GTK3 Security Maintenance

The GTK3 bindings require `glib ^0.18`. Upstream has declared that line
end-of-life and will not publish the `VariantStrIter` soundness fix described
by `RUSTSEC-2024-0429`. Solo therefore vendors the published glib 0.18.5 crate
and applies upstream's reviewed two-token fix. The source provenance and
exception lifecycle are recorded in
`vendor/glib-0.18.5-solo/SOLO-PATCH.md`.

CI verifies the patched source hash and Cargo resolution before allowing the
single version-based audit exception, rejects any other unsoundness advisory,
and runs the vulnerable iterator path in an optimized Linux test. This is a
bounded compatibility patch, not a permanent endorsement of GTK3: replace the
GTK3 stack when the tray/webview dependencies provide a production-ready GTK4
path, then remove the vendor directory and exception together.

## Action Items

1. [x] Add a Linux-only `gtk = "0.18"` dependency to `solo-tray`, matching the
       single `gtk 0.18.2` already resolved in `Cargo.lock`.
2. [x] Call `gtk::init()` before `eframe::run_native` on Linux.
3. [x] Drain pending GTK events from `update()`, bounded per frame.
4. [x] Verify on Ubuntu 24.04 against the installed package, not the build tree.
5. [x] Add headed Linux GUI coverage to CI (Xvfb or equivalent) so this class
       of defect cannot pass again — `scripts/linux_tray_gui_smoke.sh` and the
       `tray-gui-linux` job. Verified against a deliberately crashing binary
       so the guard fails when it should, rather than passing vacuously.
